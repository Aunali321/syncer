//! Applying: bringing the disk in line with the index, path by path.

use crate::{
    folder::{
        disk,
        index::Hash,
        rules::is_rule_file,
        state::{self, Work},
    },
    util::lock,
};
use anyhow::Result;
use std::sync::{Arc, atomic::Ordering};
use tokio::task::spawn_blocking;

use super::{
    jobs::{Job, Outcome, run_jobs},
    *,
};

/// What an apply round did.
#[derive(Debug, Default)]
pub struct Round {
    /// Paths brought in line.
    pub done: usize,
    /// Content to fetch from peers: (hash, a path that wants it).
    pub fetch: Vec<(Hash, String)>,
    /// Paths that failed and wait for the next round.
    pub failed: usize,
    /// An ignore file was written or removed.
    pub rules_changed: bool,
}

impl Folder {
    /// Brings every pending path in line that can be with content at hand,
    /// and lists the content to fetch.
    pub async fn apply(self: &Arc<Self>) -> Result<Round> {
        let _disk = self.disk_lock.lock().await;
        self.check()?;
        let mut rules = lock(&self.rules).clone();
        // The checkout moved (a peer's branch switch, say): what HEAD
        // tracks, and so what Git's ignore files leave out, may differ.
        if rules.follows_git() && rules.tracked().head() != self.head_commit().as_ref() {
            let before = rules.tracked().clone();
            rules = self.reload_rules().await?;
            if rules.tracked().head() != before.head() {
                lock(&self.state).pend_all();
            }
        }
        let mut round = Round::default();
        let mut jobs = Vec::new();

        {
            let mut state = lock(&self.state);
            if !state.resolve_clashes().is_empty() {
                self.dirty.store(true, Ordering::Release);
                self.seq.send_replace(state.index.seq());
            }
            let pending: Vec<String> = state.pending().iter().cloned().collect();

            for path in pending {
                let planned = state.index.get(&path).map(|entry| entry.version.clone());
                match state.work(&path, &rules) {
                    Work::Done => {
                        state.settle(&path, planned.as_ref());
                        lock(&self.problems).files.remove(&path);
                    }
                    Work::Adopt(version) => {
                        state.adopted(&path, version);
                        state.settle(&path, planned.as_ref());
                        round.done += 1;
                    }
                    Work::Place {
                        entry,
                        meta,
                        expected,
                    } => {
                        if expected.is_none()
                            && let Some(why) = refused_here(&state.disk, &path)
                        {
                            lock(&self.problems).files.insert(path, why);
                            round.failed += 1;
                            continue;
                        }
                        let source = state.disk.holding(&meta.hash, &path).map(|(at, file)| {
                            (disk::join(&self.root, at), file.stat, file.meta.kind)
                        });
                        jobs.push(Job::Place {
                            path,
                            version: entry.version,
                            meta,
                            expected,
                            source,
                        });
                    }
                    Work::Remove { version, expected } => jobs.push(Job::Remove {
                        path,
                        version,
                        expected,
                    }),
                }
            }
        }

        let (root, tmp) = (self.root.clone(), self.tmp());
        let outcomes = spawn_blocking(move || run_jobs(&root, &tmp, jobs)).await?;
        let again = {
            let mut state = lock(&self.state);
            let mut problems = lock(&self.problems);

            for (path, outcome) in outcomes {
                match outcome {
                    Outcome::Placed {
                        meta,
                        stat,
                        version,
                    } => {
                        state.placed(&path, meta, stat, version.clone());
                        state.settle(&path, Some(&version));
                        round.done += 1;
                        round.rules_changed |= is_rule_file(&path);
                        problems.files.remove(&path);
                    }
                    Outcome::Removed { version } => {
                        state.removed(&path);
                        state.settle(&path, Some(&version));
                        round.done += 1;
                        round.rules_changed |= is_rule_file(&path);
                        problems.files.remove(&path);
                    }
                    Outcome::Missing { hash, version } => {
                        state.await_content(&path, hash, &version);
                        round.fetch.push((hash, path));
                    }
                    Outcome::Failed(why) => {
                        round.failed += 1;
                        problems.files.insert(path, why);
                    }
                }
            }

            // Paths whose index entry moved while the round ran are still
            // pending; those waiting for content are not.
            state
                .pending()
                .iter()
                .any(|path| !problems.files.contains_key(path))
        };

        if round.done > 0 {
            self.dirty.store(true, Ordering::Release);
        }

        if round.rules_changed {
            self.reload_rules().await?;
            lock(&self.state).pend_all();
        }

        // A placement can wait on a removal in the same round (a file where
        // a directory goes, or the reverse): go again while rounds progress.
        if round.rules_changed || (round.done > 0 && (again || round.failed > 0)) {
            self.work.notify_one();
        }

        round.fetch.sort();
        round.fetch.dedup_by(|a, b| a.0 == b.0);
        Ok(round)
    }
}

/// Why a new file at `path` cannot be written here, if it cannot: the file
/// system would store it under another name, or take it for a file it holds.
fn refused_here(disk: &state::Disk, path: &str) -> Option<String> {
    if let Some(why) = disk.folding().refuses(path) {
        return Some(why.to_owned());
    }
    disk.clash(path)
        .map(|other| format!("clashes with {other} on this drive"))
}

#[cfg(test)]
mod tests {
    use super::{
        super::tests::{download, folder, meta},
        *,
    };
    use crate::folder::{
        index::{Entry, Version},
        scan::Scope,
    };
    use std::fs;

    #[tokio::test]
    async fn a_losing_local_edit_survives_as_its_conflict_copy() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let folder = folder(dir.path());
        // `Makefile` sorts before `Makefile (conflict ...)`: the original is
        // worked on first, and its local content must already be staged.
        fs::write(folder.root().join("Makefile"), "local").expect("write");
        folder.scan(Scope::All).await.expect("scanned");

        let theirs = Entry {
            path: "Makefile".into(),
            version: Version::default().with(99, 1),
            file: Some(meta("theirs", i64::MAX / 2)),
        };
        download(&folder, "theirs");
        folder.merge(
            &iroh::SecretKey::from_bytes(&[1; 32]).public(),
            &[theirs],
            Default::default(),
        );
        folder.apply().await.expect("applied");

        let copy = crate::folder::index::conflict_path("Makefile", &Hash::of(b"local"));
        assert_eq!(
            fs::read_to_string(folder.root().join("Makefile")).expect("read"),
            "theirs"
        );
        assert_eq!(
            fs::read_to_string(folder.root().join(&copy)).expect("the copy"),
            "local"
        );
    }

    #[tokio::test]
    async fn a_path_whose_entry_moves_during_a_round_stays_pending() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let folder = folder(dir.path());
        let peer = iroh::SecretKey::from_bytes(&[1; 32]).public();
        let first = Entry {
            path: "a".into(),
            version: Version::default().with(99, 1),
            file: Some(meta("one", 1)),
        };
        folder.merge(&peer, std::slice::from_ref(&first), Default::default());
        download(&folder, "one");

        // The round places `one`; a newer entry arrives before it settles.
        let round = folder.apply().await.expect("applied");
        assert_eq!(round.done, 1);
        let second = Entry {
            path: "a".into(),
            version: first.version.with(99, 2),
            file: Some(meta("two", 2)),
        };
        lock(&folder.state).settle("a", Some(&first.version));
        folder.merge(&peer, &[second], Default::default());
        lock(&folder.state).settle("a", Some(&first.version));
        assert!(lock(&folder.state).pending().contains("a"));

        download(&folder, "two");
        folder.apply().await.expect("applied");
        assert_eq!(
            fs::read_to_string(folder.root().join("a")).expect("read"),
            "two"
        );
    }
}
