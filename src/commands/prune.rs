use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Result};
use bytesize::ByteSize;
use chrono::{DateTime, Duration, Local};
use clap::{AppSettings, Parser};
use derive_more::Add;
use itertools::Itertools;
use log::*;
use rayon::prelude::*;

use super::{bytes, no_progress, progress_bytes, progress_counter, warm_up_wait};
use crate::backend::{DecryptReadBackend, DecryptWriteBackend, FileType, ReadBackend};
use crate::blob::{
    BlobType, BlobTypeMap, Initialize, NodeType, PackSizer, Repacker, Sum, TreeStreamerOnce,
};
use crate::commands::helpers::progress_spinner;
use crate::id::Id;
use crate::index::{IndexBackend, IndexCollector, IndexType, IndexedBackend, Indexer, ReadIndex};
use crate::repofile::{HeaderEntry, IndexBlob, IndexFile, IndexPack, SnapshotFile};
use crate::repository::OpenRepository;

#[derive(Parser)]
#[clap(global_setting(AppSettings::DeriveDisplayOrder))]
pub(super) struct Opts {
    /// Don't remove anything, only show what would be done
    #[clap(long, short = 'n')]
    pub(crate) dry_run: bool,

    /// Define maximum data to repack in % of reposize or as size (e.g. '5b', '2 kB', '3M', '4TiB') or 'unlimited'
    #[clap(long, value_name = "LIMIT", default_value = "unlimited")]
    max_repack: LimitOption,

    /// Tolerate limit of unused data in % of reposize after pruning or as size (e.g. '5b', '2 kB', '3M', '4TiB') or 'unlimited'
    #[clap(long, value_name = "LIMIT", default_value = "5%")]
    max_unused: LimitOption,

    /// Minimum duration (e.g. 90d) to keep packs before repacking or removing. More recently created
    /// packs won't be repacked or marked for deletion within this prune run.
    #[clap(long, value_name = "DURATION", default_value = "0d")]
    keep_pack: humantime::Duration,

    /// Minimum duration (e.g. 10m) to keep packs marked for deletion. More recently marked packs won't be
    /// deleted within this prune run.
    #[clap(long, value_name = "DURATION", default_value = "23h")]
    keep_delete: humantime::Duration,

    /// Delete files immediately instead of marking them. This also removes all files already marked for deletion.
    /// WARNING: Only use if you are sure the repository is not accessed by parallel processes!
    #[clap(long)]
    instant_delete: bool,

    /// Simply copy blobs when repacking instead of decrypting; possibly compressing; encrypting
    #[clap(long)]
    fast_repack: bool,

    /// Repack packs containing uncompressed blobs. This cannot be used with --fast-repack.
    /// Implies --max-unused=0.
    #[clap(long, conflicts_with = "fast-repack")]
    repack_uncompressed: bool,

    /// Only repack packs which are cacheable [default: true for a hot/cold repository, else false]
    #[clap(long, value_name = "TRUE/FALSE")]
    repack_cacheable_only: Option<bool>,

    /// Do not repack packs which only needs to be resized
    #[clap(long)]
    no_resize: bool,
}

pub(super) fn execute(repo: OpenRepository, opts: Opts, ignore_snaps: Vec<Id>) -> Result<()> {
    let be = &repo.dbe;
    if repo.config.version < 2 && opts.repack_uncompressed {
        bail!("--repack-uncompressed makes no sense for v1 repo!");
    }

    let mut index_files = Vec::new();

    let p = progress_counter("reading index...");
    let mut index_collector = IndexCollector::new(IndexType::OnlyTrees);

    for index in be.stream_all::<IndexFile>(p.clone())? {
        let (id, index) = index?;
        index_collector.extend(index.packs.clone());
        // we add the trees from packs_to_delete to the index such that searching for
        // used blobs doesn't abort if they are already marked for deletion
        index_collector.extend(index.packs_to_delete.clone());

        index_files.push((id, index));
    }
    p.finish();

    let (used_ids, total_size) = {
        let index = index_collector.into_index();
        let total_size = BlobTypeMap::init(|blob_type| index.total_size(blob_type));
        let indexed_be = IndexBackend::new_from_index(&be.clone(), index);
        let used_ids = find_used_blobs(&indexed_be, ignore_snaps)?;
        (used_ids, total_size)
    };

    // list existing pack files
    let p = progress_spinner("geting packs from repository...");
    let existing_packs: HashMap<_, _> = be.list_with_size(FileType::Pack)?.into_iter().collect();
    p.finish();

    let mut pruner = Pruner::new(used_ids, existing_packs, index_files);
    pruner.count_used_blobs();
    pruner.check()?;
    let repack_cacheable_only = opts
        .repack_cacheable_only
        .unwrap_or_else(|| repo.config.is_hot == Some(true));
    let pack_sizer = total_size.map(|tpe, size| PackSizer::from_config(&repo.config, tpe, size));
    pruner.decide_packs(
        Duration::from_std(*opts.keep_pack)?,
        Duration::from_std(*opts.keep_delete)?,
        repack_cacheable_only,
        opts.repack_uncompressed,
        &pack_sizer,
    )?;
    pruner.decide_repack(
        &opts.max_repack,
        &opts.max_unused,
        opts.repack_uncompressed,
        opts.no_resize,
        &pack_sizer,
    );
    pruner.check_existing_packs()?;
    pruner.filter_index_files(opts.instant_delete);
    pruner.print_stats();

    warm_up_wait(&repo, pruner.repack_packs().into_iter(), !opts.dry_run)?;

    if !opts.dry_run {
        pruner.do_prune(repo, opts)?;
    }
    Ok(())
}

enum LimitOption {
    Size(ByteSize),
    Percentage(u64),
    Unlimited,
}

impl FromStr for LimitOption {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s.chars().last().unwrap_or('0') {
            '%' => Self::Percentage({
                let mut copy = s.to_string();
                copy.pop();
                copy.parse()?
            }),
            'd' if s == "unlimited" => Self::Unlimited,
            _ => Self::Size(ByteSize::from_str(s).map_err(|err| anyhow!(err))?),
        })
    }
}

#[derive(Default)]
struct DeleteStats {
    remove: u64,
    recover: u64,
    keep: u64,
}

impl DeleteStats {
    fn total(&self) -> u64 {
        self.remove + self.recover + self.keep
    }
}
#[derive(Default)]
struct PackStats {
    used: u64,
    partly_used: u64,
    unused: u64, // this equals to packs-to-remove
    repack: u64,
    keep: u64,
}
#[derive(Default, Clone, Copy, Add)]
struct SizeStats {
    used: u64,
    unused: u64,
    remove: u64,
    repack: u64,
    repackrm: u64,
}

impl SizeStats {
    fn total(&self) -> u64 {
        self.used + self.unused
    }
    fn total_after_prune(&self) -> u64 {
        self.used + self.unused_after_prune()
    }
    fn unused_after_prune(&self) -> u64 {
        self.unused - self.remove - self.repackrm
    }
}

#[derive(Default)]
struct PruneStats {
    packs_to_delete: DeleteStats,
    size_to_delete: DeleteStats,
    packs: PackStats,
    blobs: BlobTypeMap<SizeStats>,
    size: BlobTypeMap<SizeStats>,
    size_unref: u64,
    index_files: u64,
}

#[derive(Debug)]
struct PruneIndex {
    id: Id,
    modified: bool,
    packs: Vec<PrunePack>,
}

impl PruneIndex {
    fn len(&self) -> usize {
        self.packs.iter().map(|p| p.blobs.len()).sum()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PackToDo {
    Undecided,
    Keep,
    Repack,
    MarkDelete,
    KeepMarked,
    Recover,
    Delete,
}

#[derive(Debug)]
struct PrunePack {
    id: Id,
    blob_type: BlobType,
    size: u32,
    delete_mark: bool,
    to_do: PackToDo,
    time: Option<DateTime<Local>>,
    blobs: Vec<IndexBlob>,
}

impl PrunePack {
    fn from_index_pack(p: IndexPack, delete_mark: bool) -> Self {
        Self {
            id: p.id,
            blob_type: p.blob_type(),
            size: p.pack_size(),
            delete_mark,
            to_do: PackToDo::Undecided,
            time: p.time,
            blobs: p.blobs,
        }
    }

    fn from_index_pack_unmarked(p: IndexPack) -> Self {
        Self::from_index_pack(p, false)
    }

    fn from_index_pack_marked(p: IndexPack) -> Self {
        Self::from_index_pack(p, true)
    }

    fn into_index_pack(self) -> IndexPack {
        IndexPack {
            id: self.id,
            time: self.time,
            size: None,
            blobs: self.blobs,
        }
    }

    fn into_index_pack_with_time(self, time: DateTime<Local>) -> IndexPack {
        IndexPack {
            id: self.id,
            time: Some(time),
            size: None,
            blobs: self.blobs,
        }
    }

    fn set_todo(&mut self, todo: PackToDo, pi: &PackInfo, stats: &mut PruneStats) {
        let tpe = self.blob_type;
        match todo {
            PackToDo::Undecided => panic!("not possible"),
            PackToDo::Keep => {
                stats.blobs[tpe].used += u64::from(pi.used_blobs);
                stats.blobs[tpe].unused += u64::from(pi.unused_blobs);
                stats.size[tpe].used += u64::from(pi.used_size);
                stats.size[tpe].unused += u64::from(pi.unused_size);
                stats.packs.keep += 1;
            }
            PackToDo::Repack => {
                stats.blobs[tpe].used += u64::from(pi.used_blobs);
                stats.blobs[tpe].unused += u64::from(pi.unused_blobs);
                stats.size[tpe].used += u64::from(pi.used_size);
                stats.size[tpe].unused += u64::from(pi.unused_size);
                stats.packs.repack += 1;
                stats.blobs[tpe].repack += u64::from(pi.unused_blobs + pi.used_blobs);
                stats.blobs[tpe].repackrm += u64::from(pi.unused_blobs);
                stats.size[tpe].repack += u64::from(pi.unused_size + pi.used_size);
                stats.size[tpe].repackrm += u64::from(pi.unused_size);
            }

            PackToDo::MarkDelete => {
                stats.blobs[tpe].unused += u64::from(pi.unused_blobs);
                stats.size[tpe].unused += u64::from(pi.unused_size);
                stats.blobs[tpe].remove += u64::from(pi.unused_blobs);
                stats.size[tpe].remove += u64::from(pi.unused_size);
            }
            PackToDo::Recover => {
                stats.packs_to_delete.recover += 1;
                stats.size_to_delete.recover += u64::from(self.size);
            }
            PackToDo::Delete => {
                stats.packs_to_delete.remove += 1;
                stats.size_to_delete.remove += u64::from(self.size);
            }
            PackToDo::KeepMarked => {
                stats.packs_to_delete.keep += 1;
                stats.size_to_delete.keep += u64::from(self.size);
            }
        }
        self.to_do = todo;
    }

    fn is_compressed(&self) -> bool {
        self.blobs
            .iter()
            .all(|blob| blob.uncompressed_length.is_some())
    }
}

#[derive(PartialEq, Eq)]
enum RepackReason {
    PartlyUsed,
    ToCompress,
    SizeMismatch,
}
use RepackReason::*;

struct Pruner {
    time: DateTime<Local>,
    used_ids: HashMap<Id, u8>,
    existing_packs: HashMap<Id, u32>,
    repack_candidates: Vec<(PackInfo, RepackReason, usize, usize)>,
    index_files: Vec<PruneIndex>,
    stats: PruneStats,
}

impl Pruner {
    fn new(
        used_ids: HashMap<Id, u8>,
        existing_packs: HashMap<Id, u32>,
        index_files: Vec<(Id, IndexFile)>,
    ) -> Self {
        let mut processed_packs = HashSet::new();
        let mut processed_packs_delete = HashSet::new();
        let mut index_files: Vec<_> = index_files
            .into_iter()
            .map(|(id, index)| {
                let mut modified = false;
                let mut packs: Vec<_> = index
                    .packs
                    .into_iter()
                    // filter out duplicate packs
                    .filter(|p| {
                        let no_duplicate = processed_packs.insert(p.id);
                        modified |= !no_duplicate;
                        no_duplicate
                    })
                    .map(PrunePack::from_index_pack_unmarked)
                    .collect();
                packs.extend(
                    index
                        .packs_to_delete
                        .into_iter()
                        // filter out duplicate packs
                        .filter(|p| {
                            let no_duplicate = processed_packs_delete.insert(p.id);
                            modified |= !no_duplicate;
                            no_duplicate
                        })
                        .map(PrunePack::from_index_pack_marked),
                );

                PruneIndex {
                    id,
                    modified,
                    packs,
                }
            })
            .collect();

        // filter out "normally" indexed packs from packs_to_delete
        for index in index_files.iter_mut() {
            let mut modified = false;
            index.packs.retain(|p| {
                !p.delete_mark || {
                    let duplicate = processed_packs.contains(&p.id);
                    modified |= duplicate;
                    !duplicate
                }
            });

            index.modified |= modified;
        }

        Self {
            time: Local::now(),
            used_ids,
            existing_packs,
            repack_candidates: Vec::new(),
            index_files,
            stats: PruneStats::default(),
        }
    }

    fn count_used_blobs(&mut self) {
        for blob in self
            .index_files
            .iter()
            .flat_map(|index| &index.packs)
            .flat_map(|pack| &pack.blobs)
        {
            if let Some(count) = self.used_ids.get_mut(&blob.id) {
                // note that duplicates are only counted up to 255. If there are more
                // duplicates, the number is set to 255. This may imply that later on
                // not the "best" pack is chosen to have that blob marked as used.
                *count = count.saturating_add(1);
            }
        }
    }

    fn check(&self) -> Result<()> {
        // check that all used blobs are present in index
        for (id, count) in &self.used_ids {
            if *count == 0 {
                error!("used blob {} is missing", id);
                bail!("missing blobs");
            }
        }
        Ok(())
    }

    fn decide_packs(
        &mut self,
        keep_pack: Duration,
        keep_delete: Duration,
        repack_cacheable_only: bool,
        repack_uncompressed: bool,
        pack_sizer: &BlobTypeMap<PackSizer>,
    ) -> Result<()> {
        // first process all marked packs then the unmarked ones:
        // - first processed packs are more likely to have all blobs seen as unused
        // - if marked packs have used blob but these blobs are all present in
        //   unmarked packs, we want to perform the deletion!
        for mark_case in [true, false] {
            for (index_num, index) in self.index_files.iter_mut().enumerate() {
                for (pack_num, pack) in index
                    .packs
                    .iter_mut()
                    .enumerate()
                    .filter(|(_, p)| p.delete_mark == mark_case)
                {
                    let pi = PackInfo::from_pack(pack, &mut self.used_ids);

                    // Various checks to determine if packs need to be kept
                    let too_young = pack.time > Some(self.time - keep_pack);
                    let keep_uncacheable = repack_cacheable_only && !pack.blob_type.is_cacheable();

                    let to_compress = repack_uncompressed && !pack.is_compressed();
                    let size_mismatch = !pack_sizer[pack.blob_type].size_ok(pack.size);

                    match (pack.delete_mark, pi.used_blobs, pi.unused_blobs) {
                        (false, 0, _) => {
                            // unused pack
                            self.stats.packs.unused += 1;
                            if too_young {
                                // keep packs which are too young
                                pack.set_todo(PackToDo::Keep, &pi, &mut self.stats);
                            } else {
                                pack.set_todo(PackToDo::MarkDelete, &pi, &mut self.stats);
                            }
                        }
                        (false, 1.., 0) => {
                            // used pack
                            self.stats.packs.used += 1;
                            if too_young || keep_uncacheable {
                                pack.set_todo(PackToDo::Keep, &pi, &mut self.stats);
                            } else if to_compress {
                                self.repack_candidates
                                    .push((pi, ToCompress, index_num, pack_num));
                            } else if size_mismatch {
                                self.repack_candidates.push((
                                    pi,
                                    SizeMismatch,
                                    index_num,
                                    pack_num,
                                ));
                            } else {
                                pack.set_todo(PackToDo::Keep, &pi, &mut self.stats);
                            }
                        }

                        (false, 1.., 1..) => {
                            // partly used pack
                            self.stats.packs.partly_used += 1;

                            if too_young || keep_uncacheable {
                                // keep packs which are too young and non-cacheable packs if requested
                                pack.set_todo(PackToDo::Keep, &pi, &mut self.stats);
                            } else {
                                // other partly used pack => candidate for repacking
                                self.repack_candidates
                                    .push((pi, PartlyUsed, index_num, pack_num));
                            }
                        }
                        (true, 0, _) => {
                            if self.time - pack.time.expect("packs_to_delete has no time")
                                >= keep_delete
                            {
                                pack.set_todo(PackToDo::Delete, &pi, &mut self.stats);
                            } else {
                                pack.set_todo(PackToDo::KeepMarked, &pi, &mut self.stats);
                            }
                        }
                        (true, 1.., _) => {
                            // needed blobs; mark this pack for recovery
                            pack.set_todo(PackToDo::Recover, &pi, &mut self.stats);
                        }
                    }
                }
            }
        }

        Ok(())
    }

    fn decide_repack(
        &mut self,
        max_repack: &LimitOption,
        max_unused: &LimitOption,
        repack_uncompressed: bool,
        no_resize: bool,
        pack_sizer: &BlobTypeMap<PackSizer>,
    ) {
        let max_unused = match (repack_uncompressed, max_unused) {
            (true, _) => 0,
            (false, LimitOption::Unlimited) => u64::MAX,
            (false, LimitOption::Size(size)) => size.as_u64(),
            // if percentag is given, we want to have
            // unused <= p/100 * size_after = p/100 * (size_used + unused)
            // which equals (1 - p/100) * unused <= p/100 * size_used
            (false, LimitOption::Percentage(p)) => (p * self.stats.size.sum().used) / (100 - p),
        };

        let max_repack = match max_repack {
            LimitOption::Unlimited => u64::MAX,
            LimitOption::Size(size) => size.as_u64(),
            LimitOption::Percentage(p) => p * self.stats.size.sum().total(),
        };

        self.repack_candidates.sort_unstable_by_key(|rc| rc.0);
        let mut resize_packs = BlobTypeMap::<Vec<_>>::default();
        let mut do_repack = BlobTypeMap::default();
        let mut repack_size = BlobTypeMap::<u64>::default();

        for (pi, repack_reason, index_num, pack_num) in std::mem::take(&mut self.repack_candidates)
        {
            let pack = &mut self.index_files[index_num].packs[pack_num];
            let blob_type = pi.blob_type;

            let total_repack_size: u64 = repack_size.into_values().sum();
            if total_repack_size + u64::from(pi.used_size) >= max_repack
                || (self.stats.size.sum().unused_after_prune() < max_unused
                    && repack_reason == PartlyUsed
                    && blob_type == BlobType::Data)
                || (repack_reason == SizeMismatch && no_resize)
            {
                pack.set_todo(PackToDo::Keep, &pi, &mut self.stats);
            } else if repack_reason == SizeMismatch {
                resize_packs[blob_type].push((pi, index_num, pack_num));
                repack_size[blob_type] += u64::from(pi.used_size);
            } else {
                pack.set_todo(PackToDo::Repack, &pi, &mut self.stats);
                repack_size[blob_type] += u64::from(pi.used_size);
                do_repack[blob_type] = true;
            }
        }
        for (blob_type, resize_packs) in resize_packs {
            // packs in resize_packs are only repacked if we anyway repack this blob type or
            // if the target pack size is reached for the blob type.
            let todo = if do_repack[blob_type]
                || repack_size[blob_type] > u64::from(pack_sizer[blob_type].pack_size())
            {
                PackToDo::Repack
            } else {
                PackToDo::Keep
            };
            for (pi, index_num, pack_num) in resize_packs {
                let pack = &mut self.index_files[index_num].packs[pack_num];
                pack.set_todo(todo, &pi, &mut self.stats);
            }
        }
    }

    fn check_existing_packs(&mut self) -> Result<()> {
        for pack in self.index_files.iter().flat_map(|index| &index.packs) {
            let existing_size = self.existing_packs.remove(&pack.id);

            // TODO: Unused Packs which don't exist (i.e. only existing in index)
            let check_size = || {
                match existing_size {
                    Some(size) if size == pack.size => Ok(()), // size is ok => continue
                    Some(size) => bail!(
                        "used pack {}: size does not match! Expected size: {}, real size: {}",
                        pack.id,
                        pack.size,
                        size
                    ),
                    None => bail!("used pack {} does not exist!", pack.id),
                }
            };

            match pack.to_do {
                PackToDo::Undecided => {
                    bail!("should not happen!")
                }
                PackToDo::Keep | PackToDo::Recover => {
                    for blob in &pack.blobs {
                        self.used_ids.remove(&blob.id);
                    }
                    check_size()?;
                }
                PackToDo::Repack => {
                    check_size()?;
                }
                PackToDo::MarkDelete | PackToDo::Delete | PackToDo::KeepMarked => {}
            }
        }

        self.used_ids.shrink_to_fit();
        self.existing_packs.shrink_to_fit();

        // all remaining packs in existing_packs are unreferenced packs
        for size in self.existing_packs.values() {
            self.stats.size_unref += u64::from(*size);
        }

        Ok(())
    }

    fn filter_index_files(&mut self, instant_delete: bool) {
        const MIN_INDEX_LEN: usize = 10_000;

        let mut any_must_modify = false;
        self.stats.index_files = self.index_files.len() as u64;
        // filter out only the index files which need processing
        self.index_files.retain(|index| {
            // index must be processed if it has been modified
            // or if any pack is not kept
            let must_modify = index.modified
                || index.packs.iter().any(|p| {
                    p.to_do != PackToDo::Keep && (instant_delete || p.to_do != PackToDo::KeepMarked)
                });

            any_must_modify |= must_modify;

            // also process index files which are too small (i.e. rebuild them)
            must_modify || index.len() < MIN_INDEX_LEN
        });

        if !any_must_modify && self.index_files.len() == 1 {
            // only one index file to process but only because it is too small
            self.index_files.clear();
        }

        // TODO: Sort index files such that files with deletes come first and files with
        // repacks come at end
    }

    fn print_stats(&self) {
        let pack_stat = &self.stats.packs;
        let blob_stat = self.stats.blobs.sum();
        let size_stat = self.stats.size.sum();

        debug!(
            "used:   {:>10} blobs, {:>10}",
            blob_stat.used,
            bytes(size_stat.used)
        );

        debug!(
            "unused: {:>10} blobs, {:>10}",
            blob_stat.unused,
            bytes(size_stat.unused)
        );
        debug!(
            "total:  {:>10} blobs, {:>10}",
            blob_stat.total(),
            bytes(size_stat.total())
        );

        println!(
            "to repack: {:>10} packs, {:>10} blobs, {:>10}",
            pack_stat.repack,
            blob_stat.repack,
            bytes(size_stat.repack)
        );
        println!(
            "this removes:                {:>10} blobs, {:>10}",
            blob_stat.repackrm,
            bytes(size_stat.repackrm)
        );
        println!(
            "to delete: {:>10} packs, {:>10} blobs, {:>10}",
            pack_stat.unused,
            blob_stat.remove,
            bytes(size_stat.remove)
        );
        if !self.existing_packs.is_empty() {
            println!(
                "unindexed: {:>10} packs,         ?? blobs, {:>10}",
                self.existing_packs.len(),
                bytes(self.stats.size_unref)
            );
        }

        println!(
            "total prune:                 {:>10} blobs, {:>10}",
            blob_stat.repackrm + blob_stat.remove,
            bytes(size_stat.repackrm + size_stat.remove + self.stats.size_unref)
        );
        println!(
            "remaining:                   {:>10} blobs, {:>10}",
            blob_stat.total_after_prune(),
            bytes(size_stat.total_after_prune())
        );
        println!(
            "unused size after prune: {:>10} ({:.2}% of remaining size)",
            bytes(size_stat.unused_after_prune()),
            size_stat.unused_after_prune() as f64 / size_stat.total_after_prune() as f64 * 100.0
        );

        println!();

        println!(
            "packs marked for deletion: {:>10}, {:>10}",
            self.stats.packs_to_delete.total(),
            bytes(self.stats.size_to_delete.total()),
        );
        println!(
            " - complete deletion:      {:>10}, {:>10}",
            self.stats.packs_to_delete.remove,
            bytes(self.stats.size_to_delete.remove),
        );
        println!(
            " - keep marked:            {:>10}, {:>10}",
            self.stats.packs_to_delete.keep,
            bytes(self.stats.size_to_delete.keep),
        );
        println!(
            " - recover:                {:>10}, {:>10}",
            self.stats.packs_to_delete.recover,
            bytes(self.stats.size_to_delete.recover),
        );

        debug!(
            "index files to rebuild: {} / {}",
            self.index_files.len(),
            self.stats.index_files
        );
    }

    fn repack_packs(&self) -> Vec<Id> {
        self.index_files
            .iter()
            .flat_map(|index| &index.packs)
            .filter(|pack| pack.to_do == PackToDo::Repack)
            .map(|pack| pack.id)
            .collect()
    }

    fn do_prune(self, repo: OpenRepository, opts: Opts) -> Result<()> {
        let be = repo.dbe;

        let indexer = Indexer::new_unindexed(be.clone()).into_shared();

        // Calculate an approximation of sizes after pruning.
        // The size actually is:
        // total_size_of_all_blobs + total_size_of_pack_headers + #packs * pack_overhead
        // This is hard/impossible to compute because:
        // - the size of blobs can change during repacking if compression is changed
        // - the size of pack headers depends on wheter blobs are compressed or not
        // - we don't know the number of packs generated by repacking
        // So, we simply use the current size of the blobs and an estimation of the pack
        // header size.

        let size_after_prune = BlobTypeMap::init(|blob_type| {
            self.stats.size[blob_type].total_after_prune()
                + self.stats.blobs[blob_type].total_after_prune()
                    * u64::from(HeaderEntry::ENTRY_LEN_COMPRESSED)
        });

        let tree_repacker = Repacker::new(
            be.clone(),
            BlobType::Tree,
            indexer.clone(),
            &repo.config,
            size_after_prune[BlobType::Tree],
        )?;

        let data_repacker = Repacker::new(
            be.clone(),
            BlobType::Data,
            indexer.clone(),
            &repo.config,
            size_after_prune[BlobType::Data],
        )?;

        // mark unreferenced packs for deletion
        if !self.existing_packs.is_empty() {
            if opts.instant_delete {
                let p = progress_counter("removing unindexed packs...");
                let existing_packs: Vec<_> = self.existing_packs.into_keys().collect();
                be.delete_list(FileType::Pack, true, existing_packs.iter(), p)?;
            } else {
                info!("marking not needed unindexed pack files for deletion...");
                for (id, size) in self.existing_packs {
                    let pack = IndexPack {
                        id,
                        size: Some(size),
                        time: Some(Local::now()),
                        blobs: Vec::new(),
                    };
                    indexer.write().unwrap().add_remove(pack)?;
                }
            }
        }

        // process packs by index_file
        let p = match (self.index_files.is_empty(), self.stats.packs.repack > 0) {
            (true, _) => {
                info!("nothing to do!");
                no_progress()
            }
            // TODO: Use a MultiProgressBar here
            (false, true) => progress_bytes("repacking // rebuilding index..."),
            (false, false) => progress_spinner("rebuilding index..."),
        };

        p.set_length(self.stats.size.sum().repack - self.stats.size.sum().repackrm);

        let mut indexes_remove = Vec::new();
        let tree_packs_remove = Arc::new(Mutex::new(Vec::new()));
        let data_packs_remove = Arc::new(Mutex::new(Vec::new()));

        let delete_pack = |pack: PrunePack| {
            // delete pack
            match pack.blob_type {
                BlobType::Data => data_packs_remove.lock().unwrap().push(pack.id),
                BlobType::Tree => tree_packs_remove.lock().unwrap().push(pack.id),
            }
        };

        let used_ids = Arc::new(Mutex::new(self.used_ids));

        let packs: Vec<_> = self
            .index_files
            .into_iter()
            .map(|index| {
                indexes_remove.push(index.id);
                index
            })
            .flat_map(|index| index.packs)
            .collect();

        packs.into_par_iter().try_for_each(|pack| {
            match pack.to_do {
                PackToDo::Undecided => bail!("pack {} got no decicion what to do", pack.id),
                PackToDo::Keep => {
                    // keep pack: add to new index
                    let pack = pack.into_index_pack();
                    indexer.write().unwrap().add(pack)?;
                }
                PackToDo::Repack => {
                    // TODO: repack in parallel
                    for blob in &pack.blobs {
                        if used_ids.lock().unwrap().remove(&blob.id).is_none() {
                            // don't save duplicate blobs
                            continue;
                        }

                        let repacker = match blob.tpe {
                            BlobType::Data => &data_repacker,
                            BlobType::Tree => &tree_repacker,
                        };
                        if opts.fast_repack {
                            repacker.add_fast(&pack.id, blob)?;
                        } else {
                            repacker.add(&pack.id, blob)?;
                        }
                        p.inc(u64::from(blob.length));
                    }
                    if opts.instant_delete {
                        delete_pack(pack);
                    } else {
                        // mark pack for removal
                        let pack = pack.into_index_pack_with_time(self.time);
                        indexer.write().unwrap().add_remove(pack)?;
                    }
                }
                PackToDo::MarkDelete => {
                    if opts.instant_delete {
                        delete_pack(pack);
                    } else {
                        // mark pack for removal
                        let pack = pack.into_index_pack_with_time(self.time);
                        indexer.write().unwrap().add_remove(pack)?;
                    }
                }
                PackToDo::KeepMarked => {
                    if opts.instant_delete {
                        delete_pack(pack);
                    } else {
                        // keep pack: add to new index
                        let pack = pack.into_index_pack();
                        indexer.write().unwrap().add_remove(pack)?;
                    }
                }
                PackToDo::Recover => {
                    // recover pack: add to new index in section packs
                    let pack = pack.into_index_pack_with_time(self.time);
                    indexer.write().unwrap().add(pack)?;
                }
                PackToDo::Delete => delete_pack(pack),
            }
            Ok(())
        })?;
        tree_repacker.finalize()?;
        data_repacker.finalize()?;
        indexer.write().unwrap().finalize()?;
        p.finish();

        // remove old index files first as they may reference pack files which are removed soon.
        if !indexes_remove.is_empty() {
            let p = progress_counter("removing old index files...");
            be.delete_list(FileType::Index, true, indexes_remove.iter(), p)?;
        }

        // get variables out of Arc<Mutex<_>>
        let data_packs_remove = data_packs_remove.lock().unwrap();
        let tree_packs_remove = tree_packs_remove.lock().unwrap();

        if !data_packs_remove.is_empty() {
            let p = progress_counter("removing old data packs...");
            be.delete_list(FileType::Pack, false, data_packs_remove.iter(), p)?;
        }

        if !tree_packs_remove.is_empty() {
            let p = progress_counter("removing old tree packs...");
            be.delete_list(FileType::Pack, true, tree_packs_remove.iter(), p)?;
        }

        Ok(())
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
struct PackInfo {
    blob_type: BlobType,
    used_blobs: u16,
    unused_blobs: u16,
    used_size: u32,
    unused_size: u32,
}

impl PartialOrd<PackInfo> for PackInfo {
    fn partial_cmp(&self, other: &PackInfo) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PackInfo {
    fn cmp(&self, other: &Self) -> Ordering {
        // first order by blob type such that tree packs are picked first
        self.blob_type.cmp(&other.blob_type).then(
            // then order such that packs with highest
            // ratio unused/used space are picked first.
            // This is equivalent to ordering by unused / total space.
            (u64::from(other.unused_size) * u64::from(self.used_size))
                .cmp(&(u64::from(self.unused_size) * u64::from(other.used_size))),
        )
    }
}

impl PackInfo {
    fn from_pack(pack: &PrunePack, used_ids: &mut HashMap<Id, u8>) -> Self {
        let mut pi = Self {
            blob_type: pack.blob_type,
            used_blobs: 0,
            unused_blobs: 0,
            used_size: 0,
            unused_size: 0,
        };

        // We search all blobs in the pack for needed ones. We do this by already marking
        // and decreasing the used blob counter for the processed blobs. If the counter
        // was decreased to 0, the blob and therefore the pack is actually used.
        // Note that by this processing, we are also able to handle duplicate blobs within a pack
        // correctly.
        // If we found a needed blob, we stop and process the information that the pack is actually needed.
        let first_needed = pack.blobs.iter().position(|blob| {
            match used_ids.get_mut(&blob.id) {
                None | Some(0) => {
                    pi.unused_size += blob.length;
                    pi.unused_blobs += 1;
                }
                Some(count) => {
                    // decrease counter
                    *count -= 1;
                    if *count == 0 {
                        // blob is actually needed
                        pi.used_size += blob.length;
                        pi.used_blobs += 1;
                        return true; // break the search
                    } else {
                        // blob is not needed
                        pi.unused_size += blob.length;
                        pi.unused_blobs += 1;
                    }
                }
            }
            false // continue with next blob
        });

        if let Some(first_needed) = first_needed {
            // The pack is actually needed.
            // We reprocess the blobs up to the first needed one and mark all blobs which are genarally needed as used.
            for blob in &pack.blobs[..first_needed] {
                match used_ids.get_mut(&blob.id) {
                    None | Some(0) => {} // already correctly marked
                    Some(count) => {
                        // remark blob as used
                        pi.unused_size -= blob.length;
                        pi.unused_blobs -= 1;
                        pi.used_size += blob.length;
                        pi.used_blobs += 1;
                        *count = 0; // count = 0 indicates to other packs that the blob is not needed anymore.
                    }
                }
            }
            // Then we process the remaining blobs and mark all blobs which are generally needed as used in this blob
            for blob in &pack.blobs[first_needed + 1..] {
                match used_ids.get_mut(&blob.id) {
                    None | Some(0) => {
                        pi.unused_size += blob.length;
                        pi.unused_blobs += 1;
                    }
                    Some(count) => {
                        // blob is used in this pack
                        pi.used_size += blob.length;
                        pi.used_blobs += 1;
                        *count = 0; // count = 0 indicates to other packs that the blob is not needed anymore.
                    }
                }
            }
        }

        pi
    }
}

// find used blobs in repo
fn find_used_blobs(
    index: &(impl IndexedBackend + Unpin),
    ignore_snaps: Vec<Id>,
) -> Result<HashMap<Id, u8>> {
    let ignore_snaps: HashSet<_> = ignore_snaps.into_iter().collect();

    let p = progress_counter("reading snapshots...");
    let list = index
        .be()
        .list(FileType::Snapshot)?
        .into_iter()
        .filter(|id| !ignore_snaps.contains(id))
        .collect();
    let snap_trees: Vec<_> = index
        .be()
        .stream_list::<SnapshotFile>(list, p.clone())?
        .into_iter()
        .map_ok(|(_, snap)| snap.tree)
        .try_collect()?;
    p.finish();

    let mut ids: HashMap<_, _> = snap_trees.iter().map(|id| (*id, 0)).collect();
    let p = progress_counter("finding used blobs...");

    let mut tree_streamer = TreeStreamerOnce::new(index.clone(), snap_trees, p)?;
    while let Some(item) = tree_streamer.next().transpose()? {
        let (_, tree) = item;
        for node in tree.nodes() {
            match node.node_type() {
                NodeType::File => ids.extend(node.content().iter().map(|id| (*id, 0))),
                NodeType::Dir => {
                    ids.insert(node.subtree().unwrap(), 0);
                }
                _ => {} // nothing to do
            }
        }
    }

    Ok(ids)
}
