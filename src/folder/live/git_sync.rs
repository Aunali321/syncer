//! The folder's repository: what this device advertises, and settling a
//! peer's Git state here.

use crate::{
    git::{self, GitState, Repo},
    ids::{DeviceId, FolderId},
    util::lock,
};
use anyhow::{Context, Result, bail};
use std::{
    fs,
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tokio::{sync::watch, task::spawn_blocking};

use super::*;

/// How long the first fetch of a repository made from a peer's may take.
const FIRST_FETCH: Duration = Duration::from_secs(5 * 60);
impl Folder {
    /// Looks at the repository for branches, tags and HEAD the user moved.
    pub async fn observe_git(self: &Arc<Self>) {
        let _disk = self.disk_lock.lock().await;
        self.observe_git_locked().await;
    }

    pub(super) async fn observe_git_locked(self: &Arc<Self>) {
        let Some(repo) = Repo::open(&self.root) else {
            self.git.send_if_modified(|state| state.take().is_some());
            return;
        };

        let mut memory = lock(&self.state).git.clone();
        let me = self.me;
        let observed = spawn_blocking(move || {
            git::sync::observe(&repo, &mut memory, me)?;
            let state = git::sync::advertise(&repo, &memory)?;
            anyhow::Ok((memory, state))
        })
        .await;

        match observed {
            Ok(Ok((memory, state))) => self.set_git(memory, state),
            Ok(Err(e)) => tracing::warn!("{}: reading the repository: {e:#}", self.id),
            Err(e) => tracing::warn!("{}: reading the repository: {e}", self.id),
        }
    }

    fn set_git(&self, memory: git::Memory, state: GitState) {
        {
            let mut current = lock(&self.state);
            if current.git != memory {
                current.git = memory;
                self.dirty.store(true, Ordering::Release);
            }
        }
        self.git.send_if_modified(|current| {
            let moved = current.as_ref() != Some(&state);
            *current = Some(state);
            moved
        });
    }

    pub fn git_state(&self) -> watch::Receiver<Option<GitState>> {
        self.git.subscribe()
    }

    pub fn head_commit(&self) -> Option<git::Oid> {
        self.git
            .borrow()
            .as_ref()
            .and_then(|state| state.head_commit.clone())
    }

    /// Settles a peer's Git state here. `fetch_pack` brings the objects it
    /// needs into the repository at the folder root: `(wants, haves)`.
    pub async fn align_git<F, Fut>(
        self: &Arc<Self>,
        peer: &DeviceId,
        theirs: &GitState,
        fetch_pack: F,
    ) -> Result<()>
    where
        F: FnOnce(Vec<git::Oid>, Vec<git::Oid>) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        let _disk = self.disk_lock.lock().await;
        let root = self.root.clone();
        let peer_state = theirs.clone();
        let (repo, wants, haves, bootstrapped) = spawn_blocking(move || {
            let mut bootstrapped = false;
            let repo = match Repo::open(&root) {
                Some(repo) => repo,
                None if fs::symlink_metadata(root.join(".git")).is_ok() => {
                    bail!("the folder's .git is not a repository directory here")
                }
                None if peer_state.head.is_some() => {
                    bootstrapped = true;
                    git::sync::bootstrap(&root, &peer_state)?
                }
                None => bail!("the peer's folder has no repository"),
            };
            let (wants, haves) = git::sync::wants(&repo, &peer_state)?;
            anyhow::Ok((repo, wants, haves, bootstrapped))
        })
        .await??;

        if !wants.is_empty() {
            fetch_pack(wants, haves)
                .await
                .context("fetching Git objects")?;
        }

        // A repository made just now remembers nothing of the one before.
        let mut memory = if bootstrapped {
            git::Memory::default()
        } else {
            lock(&self.state).git.clone()
        };
        let (label, me, theirs) = (git::sync::label(peer.as_bytes()), self.me, theirs.clone());
        let (memory, state, aligned) = spawn_blocking(move || {
            git::sync::observe(&repo, &mut memory, me)?;
            let aligned = git::sync::align(&repo, &mut memory, &theirs, &label)?;
            let state = git::sync::advertise(&repo, &memory)?;
            anyhow::Ok((memory, state, aligned))
        })
        .await??;

        let notes = GitNotes {
            diverged: aligned
                .diverged
                .iter()
                .map(|diverged| Split {
                    branch: diverged.branch.clone(),
                    merge: diverged
                        .reference
                        .strip_prefix("refs/")
                        .unwrap_or(&diverged.reference)
                        .to_owned(),
                })
                .collect(),
            notes: aligned.notes.clone(),
        };
        {
            let mut problems = lock(&self.problems);
            if notes.diverged.is_empty() && notes.notes.is_empty() {
                problems.git.remove(peer);
            } else {
                problems.git.insert(*peer, notes);
            }
        }

        if aligned.checkout_moved {
            tracing::info!("{}: checkout now at {:?}", self.id, state.head_commit);
        }

        self.set_git(memory, state);
        if bootstrapped {
            tokio::spawn(fetch_remotes(self.id.clone(), self.root.clone()));
        }
        Ok(())
    }
}

/// Fetches every remote of a repository just made from a peer's. What a
/// device last fetched is its own and does not sync, so a new repository
/// starts with none, and `git status` could not compare a branch with its
/// upstream until the first fetch.
async fn fetch_remotes(id: FolderId, root: PathBuf) {
    let Some(repo) = Repo::open(&root) else {
        return;
    };
    let mut command = tokio::process::Command::from(repo.fetch_all_command());
    command.kill_on_drop(true);
    match tokio::time::timeout(FIRST_FETCH, command.output()).await {
        Ok(Ok(output)) if output.status.success() => {
            tracing::info!("{id}: fetched from the repository's remotes");
        }
        Ok(Ok(output)) => tracing::warn!(
            "{id}: fetching from the repository's remotes: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Ok(Err(e)) => tracing::warn!("{id}: running git fetch: {e}"),
        Err(_) => tracing::warn!("{id}: git fetch took too long; run it by hand"),
    }
}
