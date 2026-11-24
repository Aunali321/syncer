//! Recording what a scan found: new, changed and deleted files become edits.

use super::{LocalFile, State};
use crate::folder::{
    disk::Stat,
    index::FileMeta,
    rules::Rules,
    scan::{Scope, Walked, within},
};

/// A path the scan must read, with what the disk record said when the walk
/// was planned. The scan's result only applies if the record still says so.
#[derive(Debug, Clone)]
pub struct ToRead {
    pub path: String,
    pub stat: Stat,
    pub seen: Option<Stat>,
}

/// A path the walk did not find, with the record it had.
#[derive(Debug, Clone)]
pub struct Gone {
    pub path: String,
    pub seen: Stat,
}

/// What a scan must do after walking.
#[derive(Debug, Default)]
pub struct ScanPlan {
    pub read: Vec<ToRead>,
    pub gone: Vec<Gone>,
    /// Records of paths the rules now leave out, or whose names cannot
    /// sync: forgotten, not deleted.
    pub excluded: Vec<String>,
}

impl State {
    /// Compares a walk with the disk record.
    pub fn plan_scan(&self, walked: &Walked, scope: &Scope, rules: &Rules) -> ScanPlan {
        let mut plan = ScanPlan::default();

        for (path, stat) in &walked.files {
            let seen = self.disk.get(path).map(|file| file.stat);
            if seen.as_ref() != Some(stat) {
                plan.read.push(ToRead {
                    path: path.clone(),
                    stat: *stat,
                    seen,
                });
            }
        }

        for (path, file) in self.disk.files() {
            if !scope.covers(path) || walked.files.contains_key(path) {
                continue;
            }
            if walked.unreadable.keys().any(|dir| within(path, dir)) {
                continue;
            }
            if rules.excluded(path, false) || walked.refused.keys().any(|at| within(path, at)) {
                plan.excluded.push(path.clone());
            } else {
                plan.gone.push(Gone {
                    path: path.clone(),
                    seen: file.stat,
                });
            }
        }

        plan
    }

    /// Records what a scan read. `read` holds each path's planned read and
    /// its content, or why it could not be read. Returns the paths whose
    /// reads must be tried again.
    pub fn apply_scan(
        &mut self,
        read: Vec<(ToRead, anyhow::Result<FileMeta>)>,
        gone: Vec<Gone>,
        excluded: Vec<String>,
    ) -> Vec<String> {
        let mut again = Vec::new();

        for (planned, result) in read {
            if self.disk.get(&planned.path).map(|file| file.stat) != planned.seen {
                // Sync wrote the path since the walk; look again.
                again.push(planned.path);
                continue;
            }
            let meta = match result {
                Ok(meta) => meta,
                Err(e) => {
                    tracing::debug!("{}: {e:#}", planned.path);
                    again.push(planned.path);
                    continue;
                }
            };
            self.record(planned.path, Some((meta, planned.stat)));
        }

        for gone in gone {
            if self.disk.get(&gone.path).map(|file| file.stat) == Some(gone.seen) {
                self.record(gone.path, None);
            }
        }

        for path in excluded {
            self.disk.remove(&path);
        }

        again
    }

    /// Records what is on disk at `path` now, as an edit when it is new
    /// content. Content the index already has for the path is adopted
    /// without an edit.
    fn record(&mut self, path: String, found: Option<(FileMeta, Stat)>) {
        let base = self
            .disk
            .get(&path)
            .map(|file| file.version.clone())
            .unwrap_or_default();
        let indexed = self.index.get(&path);

        match found {
            Some((meta, stat)) => {
                let same_as_disk = self
                    .disk
                    .get(&path)
                    .is_some_and(|file| file.meta.same_content(&meta));
                let same_as_index = indexed
                    .and_then(|entry| entry.file.as_ref())
                    .is_some_and(|file| file.same_content(&meta));
                let version = if same_as_index {
                    indexed
                        .map(|entry| entry.version.clone())
                        .unwrap_or_default()
                } else if same_as_disk {
                    // Touched, or a stat that moved without a change.
                    base
                } else {
                    let (version, changed) = self.index.edit(&path, &base, Some(meta.clone()));
                    self.pend(changed);
                    version
                };
                let meta = match self.disk.get(&path) {
                    Some(file) if file.meta.same_content(&meta) => FileMeta {
                        mtime_ms: file.meta.mtime_ms,
                        ..meta
                    },
                    _ => meta,
                };
                // Paths waiting for this content can copy it from here.
                self.content_arrived(&meta.hash);
                self.disk.insert(
                    path,
                    LocalFile {
                        meta,
                        stat,
                        version,
                    },
                );
            }
            None => {
                self.disk.remove(&path);
                let deleted_in_index = indexed.is_some_and(|entry| entry.file.is_none());
                if !deleted_in_index {
                    let (_, changed) = self.index.edit(&path, &base, None);
                    self.pend(changed);
                }
            }
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::folder::{
        disk,
        index::{Entry, Hash, Kind, Version},
        scan,
    };
    use std::fs;

    /// Scans `root` fully, reading files as the folder would.
    pub fn scan_all(state: &mut State, root: &std::path::Path) {
        let rules = Rules::new(root, true, Default::default());
        let walked = scan::walk(root, &rules, &Scope::All, Default::default());
        let plan = state.plan_scan(&walked, &Scope::All, &rules);
        let read = plan
            .read
            .into_iter()
            .map(|to_read| {
                let meta = disk::describe(&disk::join(root, &to_read.path), &to_read.stat);
                (to_read, meta)
            })
            .collect();
        let again = state.apply_scan(read, plan.gone, plan.excluded);
        assert!(again.is_empty(), "{again:?}");
    }

    fn content(state: &State, path: &str) -> Option<Hash> {
        state.index.get(path)?.file.as_ref().map(|file| file.hash)
    }

    #[test]
    fn scans_record_new_changed_and_deleted_files_once() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path();
        let mut state = State::new(1);
        fs::write(root.join("a"), "one").expect("write");
        fs::write(root.join("b"), "two").expect("write");

        scan_all(&mut state, root);
        assert_eq!(content(&state, "a"), Some(Hash::of(b"one")));
        let seq = state.index.seq();

        scan_all(&mut state, root);
        assert_eq!(
            state.index.seq(),
            seq,
            "an unchanged folder records nothing"
        );

        fs::write(root.join("a"), "uno").expect("write");
        fs::remove_file(root.join("b")).expect("remove");
        scan_all(&mut state, root);
        assert_eq!(content(&state, "a"), Some(Hash::of(b"uno")));
        assert!(
            state
                .index
                .get("b")
                .is_some_and(|entry| entry.file.is_none())
        );
        assert!(state.disk.get("b").is_none());
    }

    #[test]
    fn a_path_newly_excluded_is_forgotten_not_deleted() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path();
        fs::create_dir(root.join(".git")).expect("git dir");
        fs::write(root.join("build.log"), "x").expect("write");
        let mut state = State::new(1);
        scan_all(&mut state, root);
        assert!(state.disk.get("build.log").is_some());

        fs::write(root.join(".gitignore"), "*.log\n").expect("write");
        scan_all(&mut state, root);
        assert!(state.disk.get("build.log").is_none());
        assert!(
            state
                .index
                .get("build.log")
                .and_then(|entry| entry.file.as_ref())
                .is_some()
        );
    }

    #[test]
    fn content_already_on_disk_is_adopted_not_edited() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path();
        let mut state = State::new(1);
        let theirs = Entry {
            path: "same".into(),
            version: Version::default().with(2, 1),
            file: Some(FileMeta {
                kind: Kind::File,
                hash: Hash::of(b"shared"),
                size: 6,
                mtime_ms: 0,
                executable: false,
            }),
        };
        state.merge(std::slice::from_ref(&theirs));
        let seq = state.index.seq();

        fs::write(root.join("same"), "shared").expect("write");
        scan_all(&mut state, root);

        assert_eq!(state.index.seq(), seq, "no edit for content the index has");
        assert_eq!(
            state.disk.get("same").map(|file| file.version.clone()),
            Some(theirs.version)
        );
    }
}
