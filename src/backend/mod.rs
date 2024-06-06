use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use bytes::Bytes;
use serde::{de::DeserializeOwned, Serialize};

use crate::id::Id;

pub mod cache;
pub mod choose;
pub mod decrypt;
pub mod dry_run;
pub mod hotcold;
pub mod ignore;
pub mod local;
pub mod node;
pub mod rclone;
pub mod rest;

pub use self::ignore::*;
pub use cache::*;
pub use choose::*;
pub use decrypt::*;
pub use dry_run::*;
pub use hotcold::*;
pub use local::*;
use node::Node;
pub use rclone::*;
pub use rest::*;

/// All FileTypes which are located in separated directories
pub const ALL_FILE_TYPES: [FileType; 4] = [
    FileType::Key,
    FileType::Snapshot,
    FileType::Index,
    FileType::Pack,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileType {
    Config,
    Index,
    Key,
    Snapshot,
    Pack,
}

impl FileType {
    pub fn name(&self) -> &str {
        match &self {
            FileType::Config => "config",
            FileType::Snapshot => "snapshots",
            FileType::Index => "index",
            FileType::Key => "keys",
            FileType::Pack => "data",
        }
    }

    pub fn is_cacheable(&self) -> bool {
        match self {
            FileType::Config | FileType::Key | FileType::Pack => false,
            FileType::Snapshot | FileType::Index => true,
        }
    }
}

pub trait RepoFile: Serialize + DeserializeOwned + Sized + Send + Sync + 'static {
    const TYPE: FileType;
}

pub trait ReadBackend: Clone + Send + Sync + 'static {
    fn location(&self) -> &str;

    fn set_option(&mut self, option: &str, value: &str) -> Result<()>;

    fn list_with_size(&self, tpe: FileType) -> Result<Vec<(Id, u32)>>;

    fn list(&self, tpe: FileType) -> Result<Vec<Id>> {
        Ok(self
            .list_with_size(tpe)?
            .into_iter()
            .map(|(id, _)| id)
            .collect())
    }

    fn read_full(&self, tpe: FileType, id: &Id) -> Result<Bytes>;
    fn read_partial(
        &self,
        tpe: FileType,
        id: &Id,
        cacheable: bool,
        offset: u32,
        length: u32,
    ) -> Result<Bytes>;

    fn find_starts_with(&self, tpe: FileType, vec: &[String]) -> Result<Vec<Result<Id>>> {
        #[derive(Clone, Copy, PartialEq, Eq)]
        pub enum MapResult<T> {
            None,
            Some(T),
            NonUnique,
        }
        let mut results = vec![MapResult::None; vec.len()];
        for id in self.list(tpe)? {
            for (i, v) in vec.iter().enumerate() {
                if id.to_hex().starts_with(v) {
                    if results[i] == MapResult::None {
                        results[i] = MapResult::Some(id);
                    } else {
                        results[i] = MapResult::NonUnique;
                    }
                }
            }
        }

        Ok(results
            .into_iter()
            .enumerate()
            .map(|(i, id)| match id {
                MapResult::Some(id) => Ok(id),
                MapResult::None => Err(anyhow!("no suitable id found for {}", &vec[i])),
                MapResult::NonUnique => Err(anyhow!("id {} is not unique", &vec[i])),
            })
            .collect())
    }

    fn find_id(&self, tpe: FileType, id: &str) -> Result<Id> {
        Ok(self.find_ids(tpe, &[id.to_string()])?.remove(0))
    }

    fn find_ids(&self, tpe: FileType, ids: &[String]) -> Result<Vec<Id>> {
        let long_ids: Vec<_> = ids.iter().map(|id| Id::from_hex(id)).collect();

        Ok(match long_ids.iter().all(Result::is_ok) {
            true => long_ids.into_iter().map(Result::unwrap).collect(),
            // if the given id param are not full Ids, search for a suitable one
            false => self
                .find_starts_with(tpe, ids)?
                .into_iter()
                .collect::<Result<Vec<_>>>()?,
        })
    }
}

pub trait WriteBackend: ReadBackend {
    fn create(&self) -> Result<()>;
    fn write_bytes(&self, tpe: FileType, id: &Id, cacheable: bool, buf: Bytes) -> Result<()>;
    fn remove(&self, tpe: FileType, id: &Id, cacheable: bool) -> Result<()>;
}

pub trait ReadSource: Iterator<Item = Result<(PathBuf, Node)>> {
    type Reader: Read;
    fn read(path: &Path) -> Result<Self::Reader>;
    fn size(&self) -> Result<u64>;
}

pub trait WriteSource: Clone {
    fn create(&self, path: PathBuf, node: Node);
    fn set_metadata(&self, path: PathBuf, node: Node);
    fn write_at(&self, path: PathBuf, offset: u64, data: Bytes);
}
