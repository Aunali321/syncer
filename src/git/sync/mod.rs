//! Keeping a checkout's branches, tags and HEAD in step with peers. See
//! DESIGN.md, "Git sync". Everything here blocks.

mod align;
mod objects;

pub use align::{align, diverged};
pub use objects::{bootstrap, copy_objects, wants};

use align::clear_resolved;

use super::{
    config,
    repo::{HEADS, HeadTarget, Oid, Repo, TAGS},
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Orders HEAD changes: the higher stamp wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Stamp {
    pub lamport: u64,
    pub device: [u8; 32],
}

/// What HEAD points at, and when that was decided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadRecord {
    pub target: HeadTarget,
    pub stamp: Stamp,
}

/// What a device tells its peers about a folder's repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitState {
    pub head: Option<HeadRecord>,
    /// The commit HEAD is at, which file sync compares.
    pub head_commit: Option<Oid>,
    /// By short name.
    pub branches: BTreeMap<String, Oid>,
    pub tags: BTreeMap<String, Oid>,
    /// Deleted branches, by short name, with their tip when deleted.
    pub tombstones: BTreeMap<String, Oid>,
    /// The sanitized config a new device starts with.
    pub config: Vec<(String, String)>,
}

/// What a device remembers about its own repository between runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Memory {
    /// The branches as last seen, by short name.
    pub known: BTreeMap<String, Oid>,
    pub tombstones: BTreeMap<String, Oid>,
    pub head: Option<HeadRecord>,
    pub lamport: u64,
}

/// A diverged branch: the peer's tip, kept under a conflict ref.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diverged {
    pub branch: String,
    /// The conflict ref holding the peer's tip.
    pub reference: String,
    pub peer_tip: Oid,
}

/// What aligning with a peer did.
#[derive(Debug, Default)]
pub struct Aligned {
    /// HEAD or the checked-out branch moved, so the index was refreshed.
    pub checkout_moved: bool,
    pub diverged: Vec<Diverged>,
    /// Things left as they are, with why, for the status.
    pub notes: Vec<String>,
}

/// Reads the repository and records what the user changed since the last
/// look: deleted branches become tombstones and a HEAD that moved gets a new
/// stamp. Does nothing while a Git operation is under way. Returns whether
/// anything a peer would see changed.
pub fn observe(repo: &Repo, memory: &mut Memory, me: [u8; 32]) -> Result<bool> {
    if repo.busy().is_some() {
        return Ok(false);
    }

    let branches = short(repo.refs(HEADS)?, HEADS);
    let head = repo.head()?;

    if branches.is_empty() && !memory.known.is_empty() {
        // Every branch gone at once is a repository deleted and made again
        // (or replaced), not a user deleting branches: Git cannot delete the
        // branch it has checked out. Spreading those deletes would delete
        // every branch everywhere, so start over instead.
        *memory = Memory::default();
    }

    let mut changed = branches != memory.known;

    for (name, tip) in &memory.known {
        if !branches.contains_key(name) {
            memory.tombstones.insert(name.clone(), tip.clone());
        }
    }

    memory
        .tombstones
        .retain(|name, _| !branches.contains_key(name));
    memory.known = branches;

    match &memory.head {
        Some(record) if record.target == head => {}
        Some(_) => {
            memory.lamport += 1;
            memory.head = Some(HeadRecord {
                target: head,
                stamp: Stamp {
                    lamport: memory.lamport,
                    device: me,
                },
            });
            changed = true;
        }
        None => {
            // The checkout as it was before syncing: any decision made since
            // beats it.
            memory.head = Some(HeadRecord {
                target: head,
                stamp: Stamp {
                    lamport: 0,
                    device: me,
                },
            });
            changed = true;
        }
    }

    clear_resolved(repo)?;
    Ok(changed)
}

/// This device's state, to send to peers.
pub fn advertise(repo: &Repo, memory: &Memory) -> Result<GitState> {
    let config = repo
        .local_config()?
        .into_iter()
        .filter(|(key, value)| config::allowed(key, value))
        .collect();

    Ok(GitState {
        head: memory.head.clone(),
        head_commit: repo.head_commit()?,
        branches: memory.known.clone(),
        tags: short(repo.refs(TAGS)?, TAGS),
        tombstones: memory.tombstones.clone(),
        config,
    })
}

fn short(refs: BTreeMap<String, Oid>, prefix: &str) -> BTreeMap<String, Oid> {
    refs.into_iter()
        .filter_map(|(name, oid)| Some((name.strip_prefix(prefix)?.to_owned(), oid)))
        .collect()
}

/// A conservative check of a branch or tag name from a peer, stricter than
/// `git check-ref-format`.
pub fn valid_branch(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && !name.starts_with(['-', '/', '.'])
        && !name.ends_with(['/', '.'])
        && !name.ends_with(".lock")
        && !name.contains("..")
        && !name.contains("//")
        && !name.contains("@{")
        && !name.contains("/.")
        && name != "@"
        && name.chars().all(|c| {
            !c.is_control() && !matches!(c, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\')
        })
}

/// A peer label for conflict refs: the first characters of its id.
pub fn label(device: &[u8; 32]) -> String {
    hex::encode(&device[..5])
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::git::repo::testing::{git, repo};

    pub const A: [u8; 32] = [1; 32];
    pub const B: [u8; 32] = [2; 32];

    /// Brings `peer`'s state into `local`, objects first.
    pub fn sync(
        from: &Repo,
        from_memory: &mut Memory,
        to: &Repo,
        to_memory: &mut Memory,
    ) -> Aligned {
        observe(from, from_memory, A).expect("observed");
        observe(to, to_memory, B).expect("observed");
        let state = advertise(from, from_memory).expect("advertised");
        let (wants, haves) = wants(to, &state).expect("wants");
        copy_objects(from, to, &wants, &haves).expect("copied");
        align(to, to_memory, &state, "a").expect("aligned")
    }

    /// Writes the checked-out files, as file sync would.
    pub fn fill(repo: &Repo) {
        git(repo, &["checkout", "-q", "--", "."]);
    }

    /// Two repositories with the same first commit.
    pub fn pair() -> (tempfile::TempDir, Repo, tempfile::TempDir, Repo) {
        let (a_dir, a) = repo();
        let b_dir = tempfile::tempdir().expect("a temp dir");
        let state = {
            let mut memory = Memory::default();
            observe(&a, &mut memory, A).expect("observed");
            advertise(&a, &memory).expect("advertised")
        };
        let b = bootstrap(b_dir.path(), &state).expect("bootstrapped");
        git(&b, &["config", "user.name", "Test"]);
        git(&b, &["config", "user.email", "test@example.com"]);
        (a_dir, a, b_dir, b)
    }
}
