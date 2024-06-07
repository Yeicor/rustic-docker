use std::path::Path;

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use indicatif::ProgressBar;

use super::{progress_counter, Config};
use crate::backend::{DecryptReadBackend, FileType};
use crate::blob::{BlobType, Tree};
use crate::id::Id;
use crate::index::{IndexBackend, IndexedBackend};
use crate::repofile::SnapshotFile;
use crate::repository::OpenRepository;

#[derive(Parser)]
pub(super) struct Opts {
    #[clap(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Display a tree blob
    TreeBlob(IdOpt),
    /// Display a data blob
    DataBlob(IdOpt),
    /// Display the config file
    Config,
    /// Display an index file
    Index(IdOpt),
    /// Display a snapshot file
    Snapshot(IdOpt),
    /// Display a tree within a snapshot
    Tree(TreeOpts),
}

#[derive(Default, Parser)]
struct IdOpt {
    /// Id to display
    id: String,
}

#[derive(Parser)]
struct TreeOpts {
    /// Snapshot/path of the tree to display
    #[clap(value_name = "SNAPSHOT[:PATH]")]
    snap: String,
}

pub(super) fn execute(repo: OpenRepository, config: Config, opts: Opts) -> Result<()> {
    let be = &repo.dbe;
    match opts.command {
        Command::Config => cat_file(be, FileType::Config, IdOpt::default()),
        Command::Index(opt) => cat_file(be, FileType::Index, opt),
        Command::Snapshot(opt) => cat_file(be, FileType::Snapshot, opt),
        // special treatment for catingg blobs: read the index and use it to locate the blob
        Command::TreeBlob(opt) => cat_blob(be, BlobType::Tree, opt),
        Command::DataBlob(opt) => cat_blob(be, BlobType::Data, opt),
        // special treatment for cating a tree within a snapshot
        Command::Tree(opts) => cat_tree(be, config, opts),
    }
}

fn cat_file(be: &impl DecryptReadBackend, tpe: FileType, opt: IdOpt) -> Result<()> {
    let id = be.find_id(tpe, &opt.id)?;
    let data = be.read_encrypted_full(tpe, &id)?;
    println!("{}", String::from_utf8(data.to_vec())?);

    Ok(())
}

fn cat_blob(be: &impl DecryptReadBackend, tpe: BlobType, opt: IdOpt) -> Result<()> {
    let id = Id::from_hex(&opt.id)?;
    let data = IndexBackend::new(be, ProgressBar::hidden())?.blob_from_backend(tpe, &id)?;
    print!("{}", String::from_utf8(data.to_vec())?);

    Ok(())
}

fn cat_tree(be: &impl DecryptReadBackend, config: Config, opts: TreeOpts) -> Result<()> {
    let (id, path) = opts.snap.split_once(':').unwrap_or((&opts.snap, ""));
    let snap = SnapshotFile::from_str(
        be,
        id,
        |sn| sn.matches(&config.snapshot_filter),
        progress_counter(""),
    )?;
    let index = IndexBackend::new(be, progress_counter(""))?;
    let node = Tree::node_from_path(&index, snap.tree, Path::new(path))?;
    let id = node.subtree.ok_or_else(|| anyhow!("{path} is no dir"))?;
    let data = index.blob_from_backend(BlobType::Tree, &id)?;
    println!("{}", String::from_utf8(data.to_vec())?);

    Ok(())
}
