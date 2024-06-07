use std::collections::HashMap;
use std::num::NonZeroU32;
use std::path::PathBuf;

use anyhow::{anyhow, Result};
use clap::Parser;
use derive_getters::Dissolve;
use futures::{stream::FuturesUnordered, TryStreamExt};
use tokio::spawn;
use vlog::*;

use super::{progress_bytes, progress_counter};
use crate::backend::{DecryptReadBackend, FileType, LocalBackend};
use crate::blob::{Node, NodeStreamer, NodeType};
use crate::id::Id;
use crate::index::{IndexBackend, IndexedBackend};
use crate::repo::SnapshotFile;

#[derive(Parser)]
pub(super) struct Opts {
    /// dry-run: don't restore, only show what would be done
    #[clap(long, short = 'n')]
    dry_run: bool,

    /// TODO: remove files/dirs destination which are not contained in snapshot
    #[clap(long)]
    delete: bool,

    /// use numeric ids instead of user/groug when restoring uid/gui
    #[clap(long)]
    numeric_id: bool,

    /// snapshot to restore
    id: String,

    /// restore destination
    dest: String,
}

pub(super) async fn execute(be: &(impl DecryptReadBackend + Unpin), opts: Opts) -> Result<()> {
    let snap = SnapshotFile::from_str(be, &opts.id, |_| true, progress_counter()).await?;

    let dest = LocalBackend::new(&opts.dest);
    let index = IndexBackend::new(be, progress_counter()).await?;

    v1!("allocating dirs/files and collecting restore information...");
    let file_infos = allocate_and_collect(&dest, index.clone(), snap.tree, &opts).await?;

    v1!("restoring file contents...");
    restore_contents(be, &dest, file_infos, &opts).await?;

    v1!("setting metadata...");
    restore_metadata(&dest, index, snap.tree, &opts).await?;

    v1!("done.");
    Ok(())
}

/// allocate files, scan or remove existing files and collect restore information
async fn allocate_and_collect(
    dest: &LocalBackend,
    index: impl IndexedBackend + Unpin,
    tree: Id,
    opts: &Opts,
) -> Result<FileInfos> {
    let mut file_infos = FileInfos::new();

    let mut node_streamer = NodeStreamer::new(index.clone(), tree).await?;
    while let Some((path, node)) = node_streamer.try_next().await? {
        v3!("processing {:?}", path);
        match node.node_type() {
            NodeType::Dir => {
                if !opts.dry_run {
                    dest.create_dir(&path)?;
                }
            }
            NodeType::File => {
                // collect blobs needed for restoring
                let size = file_infos.add_file(&node, path.clone(), &index)?;
                // create the file
                if !opts.dry_run {
                    dest.create_file(&path, size)?;
                }
            }
            _ => {} // nothing to do for symlink, device, etc.
        }
    }

    Ok(file_infos)
}

/// restore_contents restores all files contents as described by file_infos
/// using the ReadBackend be and writing them into the LocalBackend dest.
async fn restore_contents(
    be: &impl DecryptReadBackend,
    dest: &LocalBackend,
    file_infos: FileInfos,
    opts: &Opts,
) -> Result<()> {
    let (filenames, restore_info) = file_infos.dissolve();

    v1!("processing blobs...");
    let p = progress_bytes();
    p.set_length(
        restore_info
            .iter()
            .flat_map(|(_, blob)| blob)
            .map(|(bl, fl)| bl.data_length() * fl.len() as u64)
            .sum(),
    );
    let mut stream = FuturesUnordered::new();

    const MAX_READER: usize = 20;
    for (pack, blob) in restore_info {
        for (bl, fls) in blob {
            let p = p.clone();
            let be = be.clone();
            let dest = dest.clone();
            let dry_run = opts.dry_run;
            let name_dests: Vec<_> = fls
                .iter()
                .map(|fl| (filenames[fl.file_idx].clone(), fl.file_start))
                .collect();

            while stream.len() > MAX_READER {
                stream.try_next().await?;
            }

            // TODO: error handling!
            stream.push(spawn(async move {
                // read pack at blob_offset with length blob_length
                let data = be
                    .read_encrypted_partial(
                        FileType::Pack,
                        &pack,
                        false,
                        bl.offset,
                        bl.length,
                        bl.uncompressed_length,
                    )
                    .await
                    .unwrap();

                if !dry_run {
                    // save into needed files in parallel
                    for (name, start) in name_dests {
                        dest.write_at(&name, start, &data).unwrap();
                        p.inc(bl.data_length());
                    }
                }
            }))
        }
    }

    stream.try_collect().await?;
    p.finish();

    Ok(())
}

async fn restore_metadata(
    dest: &LocalBackend,
    index: impl IndexedBackend + Unpin,
    tree: Id,
    opts: &Opts,
) -> Result<()> {
    // walk over tree in repository and compare with tree in dest
    let mut node_streamer = NodeStreamer::new(index, tree).await?;
    let mut dir_stack = Vec::new();
    while let Some((path, node)) = node_streamer.try_next().await? {
        if !opts.dry_run {
            match node.node_type() {
                NodeType::Dir => {
                    // set metadata for all non-parent paths in stack
                    while let Some((stackpath, _)) = dir_stack.last() {
                        if !path.starts_with(stackpath) {
                            let (path, node) = dir_stack.pop().unwrap();
                            set_metadata(dest, &path, &node, opts);
                        } else {
                            break;
                        }
                    }
                    // push current path to the stack
                    dir_stack.push((path, node));
                }
                _ => set_metadata(dest, &path, &node, opts),
            }
        }
    }

    // empty dir stack and set metadata
    for (path, node) in dir_stack.into_iter().rev() {
        set_metadata(dest, &path, &node, opts);
    }

    Ok(())
}

fn set_metadata(dest: &LocalBackend, path: &PathBuf, node: &Node, opts: &Opts) {
    v3!("processing {:?}", path);
    dest.create_special(path, node)
        .unwrap_or_else(|_| eprintln!("restore {:?}: creating special file failed.", path));
    if opts.numeric_id {
        dest.set_uid_gid(path, node.meta())
            .unwrap_or_else(|_| eprintln!("restore {:?}: setting UID/GID failed.", path));
    } else {
        dest.set_user_group(path, node.meta())
            .unwrap_or_else(|_| eprintln!("restore {:?}: setting User/Group failed.", path));
    }
    dest.set_permission(path, node.meta())
        .unwrap_or_else(|_| eprintln!("restore {:?}: chmod failed.", path));
    dest.set_times(path, node.meta())
        .unwrap_or_else(|_| eprintln!("restore {:?}: setting file times failed.", path));
}

/// struct that contains information of file contents grouped by
/// 1) pack ID,
/// 2) blob within this pack
/// 3) the actual files and position of this blob within those
#[derive(Debug, Dissolve)]
struct FileInfos {
    names: Filenames,
    r: RestoreInfo,
}

type RestoreInfo = HashMap<Id, HashMap<BlobLocation, Vec<FileLocation>>>;
type Filenames = Vec<PathBuf>;

#[derive(Debug, Hash, PartialEq, Eq)]
struct BlobLocation {
    offset: u32,
    length: u32,
    uncompressed_length: Option<NonZeroU32>,
}

impl BlobLocation {
    fn data_length(&self) -> u64 {
        match self.uncompressed_length {
            None => self.length - 32, // crypto overhead
            Some(length) => length.get(),
        }
        .into()
    }
}

#[derive(Debug)]
struct FileLocation {
    file_idx: usize,
    file_start: u64,
}

impl FileInfos {
    fn new() -> Self {
        Self {
            names: Vec::new(),
            r: HashMap::new(),
        }
    }

    /// Add the file to FilesInfos using index to get blob information.
    /// Returns the computed length of the file
    fn add_file(&mut self, file: &Node, name: PathBuf, index: &impl IndexedBackend) -> Result<u64> {
        let mut file_pos = 0;
        if !file.content().is_empty() {
            let file_idx = self.names.len();
            self.names.push(name);
            for id in file.content().iter() {
                let ie = index
                    .get_data(id)
                    .ok_or_else(|| anyhow!("did not find id {} in index", id))?;
                let bl = BlobLocation {
                    offset: *ie.offset(),
                    length: *ie.length(),
                    uncompressed_length: *ie.uncompressed_length(),
                };

                let pack = self.r.entry(*ie.pack()).or_insert_with(HashMap::new);
                let blob_location = pack.entry(bl).or_insert_with(Vec::new);
                blob_location.push(FileLocation {
                    file_idx,
                    file_start: file_pos,
                });

                file_pos += ie.data_length() as u64;
            }
        }
        Ok(file_pos)
    }
}
