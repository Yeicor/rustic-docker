use anyhow::Result;
use chrono::Local;
use clap::Parser;
use log::*;

use crate::backend::{DecryptWriteBackend, FileType};
use crate::blob::{merge_trees, BlobType, Node, Packer, Tree};
use crate::index::{IndexBackend, Indexer, ReadIndex};
use crate::repofile::{PathList, SnapshotFile, SnapshotOptions};
use crate::repository::OpenRepository;

use super::helpers::{progress_counter, progress_spinner};
use super::Config;

#[derive(Default, Parser)]
pub(super) struct Opts {
    /// Snapshots to merge. If none is given, use filter options to filter from all snapshots.
    #[clap(value_name = "ID")]
    ids: Vec<String>,

    /// Output generated snapshot in json format
    #[clap(long)]
    json: bool,

    /// Remove input snapshots after merging
    #[clap(long)]
    delete: bool,

    #[clap(flatten, next_help_heading = "Snapshot options")]
    snap_opts: SnapshotOptions,
}

pub(super) fn execute(
    repo: OpenRepository,
    config: Config,
    opts: Opts,
    command: String,
) -> Result<()> {
    let now = Local::now();
    let be = &repo.dbe;

    let snapshots = match opts.ids.is_empty() {
        true => SnapshotFile::all_from_backend(be, &config.snapshot_filter)?,
        false => SnapshotFile::from_ids(be, &opts.ids)?,
    };
    let index = IndexBackend::only_full_trees(&be.clone(), progress_counter(""))?;

    let indexer = Indexer::new(be.clone()).into_shared();
    let packer = Packer::new(
        be.clone(),
        BlobType::Tree,
        indexer.clone(),
        &repo.config,
        index.total_size(BlobType::Tree),
    )?;

    let mut snap = SnapshotFile::new_from_options(opts.snap_opts, now, command)?;

    let paths = PathList::from_strings(snapshots.iter().flat_map(|snap| snap.paths.iter()), false)?;
    snap.paths.set_paths(&paths.paths())?;

    // set snapshot time to time of latest snapshot to be merged
    snap.time = snapshots
        .iter()
        .max_by_key(|sn| sn.time)
        .map(|sn| sn.time)
        .unwrap_or(now);

    let mut summary = snap.summary.take().unwrap();
    summary.backup_start = Local::now();

    let p = progress_spinner("merging snapshots...");
    let trees = snapshots.iter().map(|sn| sn.tree).collect();

    let cmp = |n1: &Node, n2: &Node| n1.meta.mtime.cmp(&n2.meta.mtime);
    let save = |tree: Tree| {
        let (chunk, new_id) = tree.serialize()?;
        let size = u64::try_from(chunk.len())?;
        if !index.has_tree(&new_id) {
            packer.add(chunk.into(), new_id)?;
        }
        Ok((new_id, size))
    };

    let tree_merged = merge_trees(&index, trees, &cmp, &save, &mut summary)?;
    snap.tree = tree_merged;

    let stats = packer.finalize()?;
    stats.apply(&mut summary, BlobType::Tree);
    indexer.write().unwrap().finalize()?;
    p.finish();

    summary.finalize(now)?;
    snap.summary = Some(summary);

    let new_id = be.save_file(&snap)?;
    snap.id = new_id;

    if opts.json {
        let mut stdout = std::io::stdout();
        serde_json::to_writer_pretty(&mut stdout, &snap)?;
    }
    info!("saved new snapshot as {new_id}.");

    if opts.delete {
        let now = Local::now();
        let p = progress_counter("deleting old snapshots...");
        let snap_ids: Vec<_> = snapshots
            .iter()
            .filter(|sn| !sn.must_keep(now))
            .map(|sn| &sn.id)
            .collect();
        be.delete_list(FileType::Snapshot, true, snap_ids.into_iter(), p)?;
    }

    Ok(())
}
