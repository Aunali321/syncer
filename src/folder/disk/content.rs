//! Making content to place: copies that keep holes, and symlinks.

use crate::folder::index::Hash;
use anyhow::{Context, Result, bail};
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

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

/// Whether `bytes` are all zeros, which a writer skips over to leave a hole,
/// so a sparse file stays sparse. A writer that skips must end with
/// `File::set_len` for a hole at the end.
pub fn is_hole(bytes: &[u8]) -> bool {
    bytes.iter().all(|&byte| byte == 0)
}

/// Copies the file at `from` to `temp`, checking it still has `hash`.
pub fn copy_verified(from: &Path, temp: &Path, hash: &Hash) -> Result<()> {
    let mut source = File::open(from)?;
    let mut out = File::create(temp)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0; 256 * 1024];
    let mut copied = 0u64;

    loop {
        let read = source.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
        if is_hole(&buf[..read]) {
            out.seek(SeekFrom::Current(read as i64))?;
        } else {
            out.write_all(&buf[..read])?;
        }
        copied += read as u64;
    }

    if Hash::from(hasher.finalize()) != *hash {
        drop(out);
        let _ = fs::remove_file(temp);
        bail!("{} changed while it was copied", from.display());
    }

    out.set_len(copied)?;
    out.sync_all()?;
    Ok(())
}
