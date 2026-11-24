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
    write_through_temp(path, bytes, fs::OpenOptions::new())
}

/// Writes `bytes` like `write_atomic`, readable by this user only from the
/// moment the file exists.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    #[allow(unused_mut)]
    let mut options = fs::OpenOptions::new();
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    write_through_temp(path, bytes, options)
}

/// The temporary file is named for this write alone, so writers racing on
/// one path never share it: the last rename wins whole.
fn write_through_temp(path: &Path, bytes: &[u8], mut options: fs::OpenOptions) -> Result<()> {
    let mut temp = OsString::from(path);
    temp.push(format!(".{:016x}.tmp", rand::random::<u64>()));
    let temp = PathBuf::from(temp);
    let mut file = options
        .write(true)
        .create_new(true)
        .open(&temp)
        .with_context(|| format!("writing {}", temp.display()))?;
    let written = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| fs::rename(&temp, path));
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written.with_context(|| format!("writing {}", path.display()))
}

/// The canonical form of `path` when it exists, without the verbatim prefix
/// (`\\?\C:\...`) Windows gives it, which no other path compares equal to.
pub fn canonical(path: &Path) -> Option<PathBuf> {
    let real = path.canonicalize().ok()?;
    #[cfg(windows)]
    {
        let text = real.to_str()?;
        if let Some(rest) = text.strip_prefix(r"\\?\")
            && rest.as_bytes().get(1) == Some(&b':')
        {
            return Some(PathBuf::from(rest));
        }
    }
    Some(real)
}

/// An error and its causes for people, each said once: some errors already
/// include their cause in their own message.
pub fn describe(error: &anyhow::Error) -> String {
    let mut text = String::new();
    for cause in error.chain() {
        let message = cause.to_string();
        if text.contains(&message) {
            continue;
        }
        if !text.is_empty() {
            text.push_str(": ");
        }
        text.push_str(&message);
    }
    text
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
