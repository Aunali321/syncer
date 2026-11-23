//! Reading and changing files in a folder. Every write goes through a
//! temporary file and a rename, and is refused when the path no longer holds
//! what the caller last saw there. Everything here blocks.

use crate::folder::index::{FileMeta, Hash, Kind};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, Metadata},
    io::{self, ErrorKind, Read, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
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

/// Makes the directories above `path` inside `root`. Fails when one of them
/// exists as something other than a directory: a file is never replaced by
/// a directory here.
pub fn make_parents(root: &Path, path: &str) -> Result<()> {
    let mut dir = root.to_owned();
    let parts: Vec<&str> = path.split('/').collect();

    for part in &parts[..parts.len().saturating_sub(1)] {
        dir.push(part);

        match fs::symlink_metadata(&dir) {
            Ok(meta) if meta.is_dir() => continue,
            Ok(_) => bail!("{} is in the way of a directory", dir.display()),
            Err(e) if e.kind() == ErrorKind::NotFound => {
                fs::create_dir(&dir).with_context(|| format!("making {}", dir.display()))?;
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
        }
    }

    Ok(())
}

/// Moves the verified content at `temp` to `path`, with `meta`'s mode and
/// mtime, when `path` still holds what `expected` says. Gives the stat of
/// the placed file.
pub fn place(
    root: &Path,
    path: &str,
    temp: &Path,
    meta: &FileMeta,
    expected: Option<&Stat>,
) -> Result<Stat> {
    let target = join(root, path);
    make_parents(root, path)?;

    if meta.kind == Kind::File {
        let file = File::options().write(true).open(temp)?;
        set_executable(&file, meta.executable)?;
        file.set_modified(time_of(meta.mtime_ms))?;
        file.sync_all()?;
    }

    if !unchanged(&target, expected)? {
        bail!("{path} changed on disk; it is scanned again first");
    }

    // An empty directory in the way (left by a file that became a directory
    // and was emptied again; empty directories do not sync) gives way. One
    // with anything in it does not: removing it fails.
    if expected.is_none() && fs::symlink_metadata(&target).is_ok_and(|meta| meta.is_dir()) {
        remove_empty_tree(&target)
            .with_context(|| format!("{} is a directory with files in it", target.display()))?;
    }

    if let Some(Stat {
        kind: Kind::File, ..
    }) = expected
    {
        // Renaming over a file is atomic everywhere but needs the target to
        // be writable on Windows.
        clear_readonly(&target);
    }

    fs::rename(temp, &target).with_context(|| format!("moving into {}", target.display()))?;
    sync_dir(target.parent());
    stat(&target)?.with_context(|| format!("{} vanished after it was placed", target.display()))
}

/// Removes `dir` if nothing but empty directories is under it; fails, having
/// removed nothing that held a file, otherwise.
fn remove_empty_tree(dir: &Path) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            remove_empty_tree(&entry.path())?;
        } else {
            bail!("{} is in the way", entry.path().display());
        }
    }
    fs::remove_dir(dir)?;
    Ok(())
}

/// Removes the file at `path` when it still holds what `expected` says, then
/// any directories above it that it leaves empty.
pub fn remove(root: &Path, path: &str, expected: &Stat) -> Result<()> {
    let target = join(root, path);

    if !unchanged(&target, Some(expected))? {
        bail!("{path} changed on disk; it is scanned again first");
    }

    clear_readonly(&target);
    match fs::remove_file(&target) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("removing {}", target.display())),
    }

    let mut dir = target.parent();
    while let Some(current) = dir {
        if current == root || fs::remove_dir(current).is_err() {
            break;
        }
        dir = current.parent();
    }

    Ok(())
}

/// Writes `bytes` (a symlink's target) as a symlink at `temp`.
pub fn write_symlink(temp: &Path, bytes: &[u8]) -> Result<()> {
    let target = std::str::from_utf8(bytes).context("the link's target is not UTF-8")?;

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, temp)
            .with_context(|| format!("making the link {}", temp.display()))
    }

    #[cfg(not(unix))]
    {
        let _ = (temp, target);
        bail!("symlinks are not synced on this platform")
    }
}

/// Copies the file at `from` to `temp`, checking it still has `hash`.
pub fn copy_verified(from: &Path, temp: &Path, hash: &Hash) -> Result<()> {
    let mut source = File::open(from)?;
    let mut out = File::create(temp)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0; 256 * 1024];

    loop {
        let read = source.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
        out.write_all(&buf[..read])?;
    }

    if Hash::from(hasher.finalize()) != *hash {
        drop(out);
        let _ = fs::remove_file(temp);
        bail!("{} changed while it was copied", from.display());
    }

    out.sync_all()?;
    Ok(())
}

pub fn time_of(mtime_ms: i64) -> SystemTime {
    let magnitude = Duration::from_millis(mtime_ms.unsigned_abs());
    if mtime_ms >= 0 {
        UNIX_EPOCH + magnitude
    } else {
        UNIX_EPOCH - magnitude
    }
}

#[cfg(unix)]
fn set_executable(file: &File, executable: bool) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = file.metadata()?.permissions();
    let mode = permissions.mode();
    let mode = if executable {
        // Execute for whoever may read.
        mode | ((mode & 0o444) >> 2)
    } else {
        mode & !0o111
    };
    permissions.set_mode(mode);
    file.set_permissions(permissions)
}

#[cfg(not(unix))]
fn set_executable(_: &File, _: bool) -> io::Result<()> {
    Ok(())
}

fn clear_readonly(path: &Path) {
    #[cfg(windows)]
    if let Ok(meta) = fs::metadata(path) {
        let mut permissions = meta.permissions();
        if permissions.readonly() {
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            let _ = fs::set_permissions(path, permissions);
        }
    }
    #[cfg(not(windows))]
    let _ = path;
}

/// Makes a rename durable where the platform allows it.
fn sync_dir(dir: Option<&Path>) {
    #[cfg(unix)]
    if let Some(dir) = dir
        && let Ok(handle) = File::open(dir)
    {
        let _ = handle.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placing_refuses_a_path_that_changed() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path();
        fs::write(root.join("a"), "one").expect("write");
        let seen = stat(&root.join("a")).expect("stat").expect("a file");
        fs::write(root.join("a"), "two!").expect("write");

        let temp = root.join("temp");
        fs::write(&temp, "three").expect("write");
        let meta = FileMeta {
            kind: Kind::File,
            hash: Hash::of(b"three"),
            size: 5,
            mtime_ms: 1_000,
            executable: true,
        };

        assert!(place(root, "a", &temp, &meta, Some(&seen)).is_err());
        assert_eq!(fs::read_to_string(root.join("a")).expect("read"), "two!");

        let now = stat(&root.join("a")).expect("stat");
        let placed = place(root, "a", &temp, &meta, now.as_ref()).expect("placed");
        assert_eq!(fs::read_to_string(root.join("a")).expect("read"), "three");
        assert_eq!(placed.mtime_ms(), 1_000);
        #[cfg(unix)]
        assert!(placed.executable);
    }

    #[test]
    fn new_files_get_their_directories_and_removal_cleans_them_up() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path();
        let temp = root.join("temp");
        fs::write(&temp, "x").expect("write");
        let meta = FileMeta {
            kind: Kind::File,
            hash: Hash::of(b"x"),
            size: 1,
            mtime_ms: 0,
            executable: false,
        };

        let placed = place(root, "a/b/c.txt", &temp, &meta, None).expect("placed");
        assert!(root.join("a/b/c.txt").is_file());

        remove(root, "a/b/c.txt", &placed).expect("removed");
        assert!(!root.join("a").exists());
        assert!(root.exists());
    }

    #[test]
    fn an_empty_directory_gives_way_to_a_file_and_a_full_one_does_not() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path();
        let meta = FileMeta {
            kind: Kind::File,
            hash: Hash::of(b"x"),
            size: 1,
            mtime_ms: 0,
            executable: false,
        };
        fs::create_dir_all(root.join("empty/nested/deeper")).expect("mkdir");
        fs::create_dir_all(root.join("full/inner")).expect("mkdir");
        fs::write(root.join("full/inner/file"), "keep").expect("write");
        for name in ["empty", "full"] {
            fs::write(root.join("temp"), "x").expect("write");
            let placed = place(root, name, &root.join("temp"), &meta, None);
            assert_eq!(placed.is_ok(), name == "empty", "{name}");
        }
        assert!(root.join("empty").is_file());
        assert!(root.join("full/inner").is_dir());
    }

    #[test]
    fn a_file_is_never_replaced_by_a_directory() {
        let dir = tempfile::tempdir().expect("a temp dir");
        fs::write(dir.path().join("a"), "file").expect("write");

        assert!(make_parents(dir.path(), "a/b").is_err());
        assert!(dir.path().join("a").is_file());
    }

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
