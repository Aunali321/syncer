//! Small helpers shared across modules.

use anyhow::{Context, Result};
use std::{
    ffi::OsString,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard, PoisonError},
};

/// Locks `mutex`, carrying on with what a holder that panicked left.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Writes `bytes` to `path` whole or not at all: through a temporary file
/// beside it, flushed, then renamed over it.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut temp = OsString::from(path);
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    let mut file =
        fs::File::create(&temp).with_context(|| format!("writing {}", temp.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temp, path).with_context(|| format!("writing {}", path.display()))
}

/// Writes `bytes` like `write_atomic`, readable by this user only.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    write_atomic(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// A byte count for people: `1.5 MB`.
pub fn bytes(count: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut value = count as f64;
    let mut unit = 0;

    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{count} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn byte_counts_read_well() {
        assert_eq!(super::bytes(999), "999 B");
        assert_eq!(super::bytes(1_500_000), "1.5 MB");
    }
}
