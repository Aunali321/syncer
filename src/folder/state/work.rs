//! What each path needs to match the index, and paths that are a file and
//! a directory at once.

use super::State;
use crate::folder::{
    disk::Stat,
    index::{Entry, FileMeta, Version},
    rules::Rules,
};
use std::collections::BTreeSet;

/// What to do for one path to bring the disk in line with the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Work {
    /// The disk already matches.
    Done,
    /// The disk has the target content but an older version: record it.
    Adopt(Version),
    /// Write `entry`'s file at its path, which holds `expected` now.
    Place {
        entry: Entry,
        meta: FileMeta,
        expected: Option<Stat>,
    },
    /// Delete the file, which holds `expected` now.
    Remove { version: Version, expected: Stat },
}

impl State {
    /// Settles paths that are live as a file and as a directory at once:
    /// one device replaced a file with a directory while another edited the
    /// file, say. No file system holds both, so the directory wins and the
    /// file moves to a conflict copy beside it. Every device that sees the
    /// clash records the same copy and a deletion, so the indexes converge
    /// whichever does it first. Returns the paths that changed.
    pub fn resolve_clashes(&mut self) -> Vec<String> {
        let mut clashes = BTreeSet::new();

        for path in &self.pending {
            if !self.index.is_live(path) {
                continue;
            }
            if self.index.live_under(path) {
                clashes.insert(path.clone());
            }
            let mut at = 0;
            while let Some(slash) = path[at..].find('/') {
                let parent = &path[..at + slash];
                if self.index.is_live(parent) {
                    clashes.insert(parent.to_owned());
                }
                at += slash + 1;
            }
        }

        let mut changed = Vec::new();
        for path in clashes {
            let Some(entry) = self.index.get(&path).cloned() else {
                continue;
            };
            let Some(file) = entry.file else { continue };
            let copy = Entry {
                path: crate::folder::index::conflict_path(&path, &file.hash),
                version: Version::default(),
                file: Some(file),
            };
            changed.extend(self.index.merge(&copy));
            let (_, deleted) = self.index.edit(&path, &entry.version, None);
            changed.extend(deleted);
        }

        self.pend(changed.iter().cloned());
        changed
    }

    /// What `path` needs to match the index.
    pub fn work(&self, path: &str, rules: &Rules) -> Work {
        let entry = self.index.get(path);
        let local = self.disk.get(path);

        match (entry.and_then(|entry| entry.file.as_ref()), local) {
            (None, None) => Work::Done,
            (Some(target), Some(local)) if local.meta.same_content(target) => {
                let version = entry.map(|entry| entry.version.clone()).unwrap_or_default();
                if local.version == version {
                    Work::Done
                } else {
                    Work::Adopt(version)
                }
            }
            _ if rules.excluded(path, false) => Work::Done,
            (Some(target), local) => match entry {
                Some(entry) => Work::Place {
                    entry: entry.clone(),
                    meta: target.clone(),
                    expected: local.map(|file| file.stat),
                },
                None => Work::Done,
            },
            (None, Some(local)) => match entry {
                Some(entry) => Work::Remove {
                    version: entry.version.clone(),
                    expected: local.stat,
                },
                // Known on disk but never in the index: the next scan records it.
                None => Work::Done,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{super::scan::tests::scan_all, *};
    use crate::folder::index::{Hash, Kind};
    use std::fs;

    #[test]
    fn work_follows_the_index() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path();
        fs::write(root.join("keep"), "same").expect("write");
        fs::write(root.join("drop"), "bye").expect("write");
        let mut state = State::new(1);
        scan_all(&mut state, root);
        let rules = Rules::new(root, true, Default::default());

        let theirs = Entry {
            path: "new".into(),
            version: Version::default().with(2, 1),
            file: Some(FileMeta {
                kind: Kind::File,
                hash: Hash::of(b"hello"),
                size: 5,
                mtime_ms: 0,
                executable: false,
            }),
        };
        let drop_version = state
            .index
            .get("drop")
            .map(|entry| entry.version.with(2, 1))
            .unwrap_or_default();
        let delete = Entry {
            path: "drop".into(),
            version: drop_version,
            file: None,
        };
        let changed = state.merge(&[theirs.clone(), delete]);

        assert_eq!(changed, ["new", "drop"]);
        assert_eq!(state.work("keep", &rules), Work::Done);
        assert!(matches!(
            state.work("new", &rules),
            Work::Place { expected: None, .. }
        ));
        assert!(matches!(state.work("drop", &rules), Work::Remove { .. }));
    }

    #[test]
    fn a_file_where_a_directory_lives_moves_aside() {
        let mut state = State::new(1);
        let meta = |byte: u8| FileMeta {
            kind: Kind::File,
            hash: Hash([byte; 32]),
            size: 1,
            mtime_ms: 0,
            executable: false,
        };
        let file = Entry {
            path: "x".into(),
            version: Version::default().with(2, 1),
            file: Some(meta(1)),
        };
        let inside = Entry {
            path: "x/y".into(),
            version: Version::default().with(3, 1),
            file: Some(meta(2)),
        };
        state.merge(&[file, inside]);

        let changed = state.resolve_clashes();
        assert!(!changed.is_empty());
        assert!(!state.index.is_live("x"));
        assert!(state.index.is_live("x/y"));
        assert!(state.index.is_live("x (conflict 01010101)"));
        assert!(state.resolve_clashes().is_empty(), "settled once");
    }
}
