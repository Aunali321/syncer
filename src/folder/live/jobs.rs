//! Carrying out an apply round's changes: staging content, then placing and
//! removing files. Everything here blocks.

use crate::folder::{
    disk,
    index::{FileMeta, Hash, Kind, Version},
};
use anyhow::{Context, Result, bail};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// A change to the disk, worked out under the state lock and carried out
/// without it.
pub(super) enum Job {
    Place {
        path: String,
        version: Version,
        meta: FileMeta,
        expected: Option<disk::Stat>,
        /// A file here with the same content, to copy from.
        source: Option<(PathBuf, disk::Stat, Kind)>,
    },
    Remove {
        path: String,
        version: Version,
        expected: disk::Stat,
    },
}

pub(super) enum Outcome {
    Placed {
        meta: FileMeta,
        stat: disk::Stat,
        version: Version,
    },
    Removed {
        version: Version,
    },
    /// The content is not here: fetch it. `version` is the entry's the work
    /// was planned for.
    Missing {
        hash: Hash,
        version: Version,
    },
    Failed(String),
}

/// Carries out a round's jobs. Every placement's content is staged in `tmp`
/// before any file changes, so content one path still needs is never lost to
/// another path's overwrite (a conflict copy and the file it was taken from,
/// say). Removals go last, so a renamed file is copied before its old path
/// goes. Everything here blocks.
pub(super) fn run_jobs(root: &Path, tmp: &Path, jobs: Vec<Job>) -> Vec<(String, Outcome)> {
    let mut uses: std::collections::HashMap<Hash, usize> = std::collections::HashMap::new();

    for job in &jobs {
        if let Job::Place { meta, source, .. } = job {
            *uses.entry(meta.hash).or_default() += 1;
            let staged = tmp.join(meta.hash.to_string());
            if !staged.exists()
                && let Some((from, stat, kind)) = source
                && let Err(e) = stage(from, stat, *kind, &meta.hash, &staged)
            {
                tracing::debug!("staging {}: {e:#}", from.display());
            }
        }
    }

    let mut outcomes = Vec::with_capacity(jobs.len());
    let mut removals = Vec::new();

    for job in jobs {
        match job {
            Job::Place {
                path,
                version,
                meta,
                expected,
                ..
            } => {
                let staged = tmp.join(meta.hash.to_string());
                let outcome = if staged.exists() {
                    let last = uses.get_mut(&meta.hash).map(|count| {
                        *count -= 1;
                        *count == 0
                    });
                    match place_staged(
                        root,
                        &path,
                        &staged,
                        &meta,
                        expected.as_ref(),
                        last == Some(true),
                    ) {
                        Ok(stat) => Outcome::Placed {
                            meta,
                            stat,
                            version,
                        },
                        Err(e) => Outcome::Failed(crate::util::describe(&e)),
                    }
                } else {
                    Outcome::Missing {
                        hash: meta.hash,
                        version,
                    }
                };
                outcomes.push((path, outcome));
            }
            Job::Remove {
                path,
                version,
                expected,
            } => removals.push((path, version, expected)),
        }
    }

    for (path, version, expected) in removals {
        let outcome = match disk::remove(root, &path, &expected) {
            Ok(()) => Outcome::Removed { version },
            Err(e) => Outcome::Failed(crate::util::describe(&e)),
        };
        outcomes.push((path, outcome));
    }

    outcomes
}

/// Copies content from a file here into `staged`, checking its hash.
fn stage(from: &Path, stat: &disk::Stat, kind: Kind, hash: &Hash, staged: &Path) -> Result<()> {
    if !disk::unchanged(from, Some(stat))? {
        bail!("changed since it was scanned");
    }
    let partial = staged.with_extension("stage");
    match kind {
        Kind::File => disk::copy_verified(from, &partial, hash)?,
        Kind::Symlink => {
            let bytes = disk::link_bytes(from)?;
            if Hash::of(&bytes) != *hash {
                bail!("the link changed since it was scanned");
            }
            fs::write(&partial, bytes)?;
        }
    }
    fs::rename(&partial, staged)?;
    Ok(())
}

/// Places staged content at `path`. The staged copy is moved on its last
/// use and copied before that; it stays when placement fails.
fn place_staged(
    root: &Path,
    path: &str,
    staged: &Path,
    meta: &FileMeta,
    expected: Option<&disk::Stat>,
    last_use: bool,
) -> Result<disk::Stat> {
    let content = staged.with_extension("place");

    if meta.kind == Kind::Symlink {
        let target = fs::read(staged)?;
        let _ = fs::remove_file(&content);
        disk::write_symlink(&content, &target)?;
    } else if last_use {
        fs::rename(staged, &content)?;
    } else {
        disk::copy_verified(staged, &content, &meta.hash).context("copying staged content")?;
    }

    let placed = disk::place(root, path, &content, meta, expected);
    if placed.is_err() {
        if meta.kind == Kind::File && last_use {
            let _ = fs::rename(&content, staged);
        } else {
            let _ = fs::remove_file(&content);
        }
    } else if meta.kind == Kind::Symlink && last_use {
        let _ = fs::remove_file(staged);
    }
    placed
}
