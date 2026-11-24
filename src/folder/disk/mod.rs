//! Reading and changing files in a folder. Every write goes through a
//! temporary file and a rename, and is refused when the path no longer holds
//! what the caller last saw there. Everything here blocks.

mod content;
mod folding;
mod write;

pub use content::{copy_verified, is_hole, write_symlink};
pub use folding::Folding;
pub use write::{make_parents, place, remove};

use crate::folder::index::{FileMeta, Hash, Kind};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, Metadata},
    io::{self, ErrorKind, Read},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// What the file system says about a path, enough to tell whether it changed
/// without reading it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stat {
    pub kind: Kind,
    pub size: u64,
    pub mtime_ns: i128,
    /// 0 where the platform has none.
    pub inode: u64,
    /// When the inode last changed, which any write moves even when the
    /// writer puts the mtime back. 0 where the platform has none.
    #[serde(default)]
    pub ctime_ns: i128,
    pub executable: bool,
}

impl Stat {
    /// `None` for anything that is neither a file nor a symlink.
    pub fn of(meta: &Metadata) -> Option<Self> {
        let file_type = meta.file_type();
        let kind = if file_type.is_symlink() {
            Kind::Symlink
        } else if file_type.is_file() {
            Kind::File
        } else {
            return None;
        };

        Some(Self {
            kind,
            size: meta.len(),
            mtime_ns: meta.modified().map(nanos).unwrap_or(0),
            inode: inode(meta),
            ctime_ns: ctime(meta),
            executable: kind == Kind::File && executable(meta),
        })
    }

    pub fn mtime_ms(&self) -> i64 {
        i64::try_from(self.mtime_ns.div_euclid(1_000_000)).unwrap_or(i64::MAX)
    }
}

fn nanos(time: SystemTime) -> i128 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => i128::try_from(after.as_nanos()).unwrap_or(i128::MAX),
        Err(e) => -i128::try_from(e.duration().as_nanos()).unwrap_or(i128::MAX),
    }
}

#[cfg(unix)]
fn inode(meta: &Metadata) -> u64 {
    std::os::unix::fs::MetadataExt::ino(meta)
}

#[cfg(not(unix))]
fn inode(_: &Metadata) -> u64 {
    0
}

#[cfg(unix)]
fn ctime(meta: &Metadata) -> i128 {
    use std::os::unix::fs::MetadataExt;
    i128::from(meta.ctime()) * 1_000_000_000 + i128::from(meta.ctime_nsec())
}

#[cfg(not(unix))]
fn ctime(_: &Metadata) -> i128 {
    0
}

#[cfg(unix)]
fn executable(meta: &Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_: &Metadata) -> bool {
    false
}

/// The stat of `path` without following a final symlink, or `None` when
/// nothing syncable is there.
pub fn stat(path: &Path) -> io::Result<Option<Stat>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(Stat::of(&meta)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Reads and hashes the file or symlink at `path`, which `stat` described.
/// Fails when the path changed while it was read, so a file being written is
/// looked at again later rather than recorded half done.
pub fn describe(path: &Path, stat: &Stat) -> Result<FileMeta> {
    let hash = match stat.kind {
        Kind::Symlink => Hash::of(link_bytes(path)?.as_slice()),
        Kind::File => hash_file(path)?,
    };
    let after = self::stat(path)?.context("removed while it was read")?;

    if after != *stat {
        bail!("changed while it was read");
    }

    Ok(FileMeta {
        kind: stat.kind,
        hash,
        size: stat.size,
        mtime_ms: stat.mtime_ms(),
        executable: stat.executable,
    })
}

pub fn hash_file(path: &Path) -> Result<Hash> {
    let mut hasher = blake3::Hasher::new();
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut buf = vec![0; 256 * 1024];

    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }

    Ok(hasher.finalize().into())
}

/// A symlink's target as bytes, the content syncer sends for it.
pub fn link_bytes(path: &Path) -> Result<Vec<u8>> {
    let target =
        fs::read_link(path).with_context(|| format!("reading the link {}", path.display()))?;
    target
        .to_str()
        .map(|text| text.as_bytes().to_vec())
        .context("the link's target is not UTF-8")
}

/// Joins a `/`-separated folder path onto `root`.
pub fn join(root: &Path, path: &str) -> PathBuf {
    let mut out = root.to_owned();
    out.extend(path.split('/'));
    out
}

/// Whether `path` holds what `expected` says: the same stat, or nothing when
/// `expected` is `None`.
pub fn unchanged(path: &Path, expected: Option<&Stat>) -> Result<bool> {
    let now = stat(path).with_context(|| format!("reading {}", path.display()))?;

    Ok(match (now, expected) {
        (None, None) => true,
        (Some(now), Some(expected)) => now == *expected,
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_hashes_content() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("f");
        fs::write(&path, "hello").expect("write");
        let seen = stat(&path).expect("stat").expect("a file");

        let meta = describe(&path, &seen).expect("described");
        assert_eq!(meta.hash, Hash::of(b"hello"));
        assert_eq!(meta.size, 5);
    }
}
