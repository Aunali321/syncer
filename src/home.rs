//! Where syncer keeps its key, settings and folder state on this device.

use crate::{
    ids::FolderId,
    util::{write_atomic, write_private},
};
use anyhow::{Context, Result};
use iroh::SecretKey;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

/// The data directory.
#[derive(Debug, Clone)]
pub struct Home {
    dir: PathBuf,
}

impl Home {
    /// `dir`, else `$SYNCER_HOME`, else the platform's data directory.
    pub fn resolve(dir: Option<PathBuf>) -> Result<Self> {
        let dir = match dir.or_else(|| std::env::var_os("SYNCER_HOME").map(PathBuf::from)) {
            Some(dir) => dir,
            None => dirs::data_local_dir()
                .context("no data directory on this platform; pass --home")?
                .join("syncer"),
        };
        Ok(Self { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn create(&self) -> Result<()> {
        fs::create_dir_all(self.dir.join("folders"))
            .with_context(|| format!("making {}", self.dir.display()))
    }

    pub fn key(&self) -> PathBuf {
        self.dir.join("key")
    }

    pub fn cluster(&self) -> PathBuf {
        self.dir.join("cluster.json")
    }

    pub fn config(&self) -> PathBuf {
        self.dir.join("config.json")
    }

    /// The UDP port of the last run.
    pub fn port(&self) -> PathBuf {
        self.dir.join("port")
    }

    pub fn addrs(&self) -> PathBuf {
        self.dir.join("addresses.json")
    }

    /// Where the running service says how to reach it.
    pub fn control(&self) -> PathBuf {
        self.dir.join("control.json")
    }

    pub fn folder_state(&self, id: &FolderId) -> PathBuf {
        self.dir.join("folders").join(format!("{id}.state"))
    }

    /// This device's secret key, made on first use.
    pub fn secret_key(&self) -> Result<SecretKey> {
        let path = self.key();

        match fs::read(&path) {
            Ok(bytes) => {
                let bytes: [u8; 32] = bytes
                    .as_slice()
                    .try_into()
                    .with_context(|| format!("{} is not a key", path.display()))?;
                Ok(SecretKey::from_bytes(&bytes))
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {
                let key = SecretKey::from_bytes(&rand::random());
                write_private(&path, &key.to_bytes())?;
                Ok(key)
            }
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn read_json<T: for<'de> Deserialize<'de>>(&self, path: &Path) -> Result<Option<T>> {
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("reading {}", path.display()))
                .map(Some),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn write_json<T: Serialize>(&self, path: &Path, value: &T) -> Result<()> {
        write_atomic(path, &serde_json::to_vec_pretty(value)?)
    }
}

/// Settings that belong to this device only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalConfig {
    /// The folders this device holds, by id.
    pub folders: BTreeMap<FolderId, LocalFolder>,
    /// Where offered folders are accepted without asking, if anywhere.
    pub auto_accept: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalFolder {
    pub path: PathBuf,
    pub paused: bool,
}

/// This machine's name, for a new device.
pub fn hostname() -> String {
    let from_env = ["HOSTNAME", "COMPUTERNAME"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok());
    let from_file = || fs::read_to_string("/etc/hostname").ok();

    from_env
        .or_else(from_file)
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "device".to_owned())
}
