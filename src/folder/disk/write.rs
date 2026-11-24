//! Changing files: placing verified content, removing, copying. Every
//! change checks first that the path still holds what was last seen there.

use super::{Stat, join, stat, unchanged};
use crate::folder::index::{FileMeta, Kind};
use anyhow::{Context, Result, bail};
use std::{
    fs::{self, File},
    io::{self, ErrorKind},
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

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
        bail!("file changed mid-sync (retrying)");
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
        bail!("file changed mid-sync (retrying)");
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
    use crate::folder::index::Hash;

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
}
