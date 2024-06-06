use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::backend::{FileType, RepoFile};
use crate::id::Id;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ConfigFile {
    pub version: u32,
    pub id: Id,
    pub chunker_polynomial: String,
}

impl RepoFile for ConfigFile {
    const TYPE: FileType = FileType::Config;
}

impl ConfigFile {
    pub fn new(version: u32, id: Id, poly: u64) -> Self {
        Self {
            version,
            id,
            chunker_polynomial: format!("{:x}", poly),
        }
    }

    pub fn poly(&self) -> Result<u64> {
        Ok(u64::from_str_radix(&self.chunker_polynomial, 16)?)
    }
}
