//! What a device keeps about one folder: the index (where the folder should
//! be), the disk record (where it is), the Git memory and the peers' cursors.
//! The logic here is synchronous and does no I/O beyond what callers pass
//! in, so it can be tested on its own. See DESIGN.md, "What is on disk".

use crate::{
    folder::{
        disk::Stat,
        index::{Entry, FileMeta, Hash, Index, Replica, Version},
        rules::Rules,
        scan::{Scope, Walked, within},
    },
    git,
    ids::DeviceId,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// One path as this device last saw it on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalFile {
    pub meta: FileMeta,
    pub stat: Stat,
    /// The version of the content on disk.
    pub version: Version,
}

/// The files on disk as last seen, with lookups by hash (to copy content
/// locally) and by lowercased path (to catch names that differ only by case).
#[derive(Debug, Default)]
pub struct Disk {
    files: BTreeMap<String, LocalFile>,
    by_hash: HashMap<Hash, BTreeSet<String>>,
    by_lower: HashMap<String, BTreeSet<String>>,
}

impl Disk {
    fn from_files(files: BTreeMap<String, LocalFile>) -> Self {
        let mut disk = Self::default();
        for (path, file) in files {
            disk.insert(path, file);
        }
        disk
    }

    pub fn get(&self, path: &str) -> Option<&LocalFile> {
        self.files.get(path)
    }

    pub fn files(&self) -> &BTreeMap<String, LocalFile> {
        &self.files
    }

    pub fn insert(&mut self, path: String, file: LocalFile) {
        self.remove(&path);
        self.by_hash
            .entry(file.meta.hash)
            .or_default()
            .insert(path.clone());
        self.by_lower
            .entry(path.to_lowercase())
            .or_default()
            .insert(path.clone());
        self.files.insert(path, file);
    }

    pub fn remove(&mut self, path: &str) -> Option<LocalFile> {
        let file = self.files.remove(path)?;
        drop_from(&mut self.by_hash, &file.meta.hash, path);
        drop_from(&mut self.by_lower, &path.to_lowercase(), path);
        Some(file)
    }

    /// A path on disk holding content `hash`, `preferred` first.
    pub fn holding(&self, hash: &Hash, preferred: &str) -> Option<(&str, &LocalFile)> {
        let paths = self.by_hash.get(hash)?;
        let path = match paths.get(preferred) {
            Some(path) => path,
            None => paths.iter().next()?,
        };
        self.files
            .get_key_value(path)
            .map(|(path, file)| (path.as_str(), file))
    }

    /// Another path on disk whose name differs from `path` only by case.
    pub fn case_clash(&self, path: &str) -> Option<&str> {
        self.by_lower
            .get(&path.to_lowercase())?
            .iter()
            .find(|other| *other != path)
            .map(String::as_str)
    }
}

fn drop_from<K: std::hash::Hash + Eq>(map: &mut HashMap<K, BTreeSet<String>>, key: &K, path: &str) {
    if let Some(paths) = map.get_mut(key) {
        paths.remove(path);
        if paths.is_empty() {
            map.remove(key);
        }
    }
}

/// How far this device has read a peer's index.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub replica: Replica,
    pub seq: u64,
}

/// What is saved between runs.
#[derive(Serialize, Deserialize)]
struct Saved {
    index: Index,
    disk: BTreeMap<String, LocalFile>,
    cursors: BTreeMap<DeviceId, Cursor>,
    git: git::Memory,
}

/// One folder's state on this device.
pub struct State {
    pub index: Index,
    pub disk: Disk,
    pub cursors: BTreeMap<DeviceId, Cursor>,
    pub git: git::Memory,
    /// Paths where the index and the disk may differ.
    pending: BTreeSet<String>,
}

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
    /// Records of paths the rules now leave out: forgotten, not deleted.
    pub excluded: Vec<String>,
}

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
    pub fn new(replica: Replica) -> Self {
        Self {
            index: Index::new(replica),
            disk: Disk::default(),
            cursors: BTreeMap::new(),
            git: git::Memory::default(),
            pending: BTreeSet::new(),
        }
    }

    pub fn encode(&self) -> anyhow::Result<Vec<u8>> {
        #[derive(Serialize)]
        struct SavedRef<'a> {
            index: &'a Index,
            disk: &'a BTreeMap<String, LocalFile>,
            cursors: &'a BTreeMap<DeviceId, Cursor>,
            git: &'a git::Memory,
        }

        Ok(rmp_serde::to_vec_named(&SavedRef {
            index: &self.index,
            disk: &self.disk.files,
            cursors: &self.cursors,
            git: &self.git,
        })?)
    }

    pub fn decode(bytes: &[u8]) -> anyhow::Result<Self> {
        let saved: Saved = rmp_serde::from_slice(bytes)?;
        let mut state = Self {
            index: saved.index.loaded(),
            disk: Disk::from_files(saved.disk),
            cursors: saved.cursors,
            git: saved.git,
            pending: BTreeSet::new(),
        };
        state.pend_all();
        Ok(state)
    }

    /// Whether this device ever synced the folder: then an empty or missing
    /// folder means trouble, not a fresh start.
    pub fn has_history(&self) -> bool {
        !self.disk.files.is_empty() || !self.index.is_empty()
    }

    /// Forgets what was on disk, so a fresh scan deletes nothing. The index,
    /// where the folder should be, stays.
    pub fn forget_disk(&mut self) {
        self.disk = Disk::default();
        self.pend_all();
    }

    /// Marks every path for a look, as when the rules changed and paths
    /// they left out may sync now.
    pub fn pend_all(&mut self) {
        self.pending = self
            .index
            .entries()
            .map(|entry| entry.path.clone())
            .chain(self.disk.files.keys().cloned())
            .collect();
    }

    pub fn pending(&self) -> &BTreeSet<String> {
        &self.pending
    }

    pub fn pend(&mut self, paths: impl IntoIterator<Item = String>) {
        self.pending.extend(paths);
    }

    /// Takes `path` off the pending list if its index entry is still at
    /// `planned`, the version the finished work was planned for. An entry
    /// that moved meanwhile keeps the path pending.
    pub fn settle(&mut self, path: &str, planned: Option<&Version>) {
        if self.index.get(path).map(|entry| &entry.version) == planned {
            self.pending.remove(path);
        }
    }

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

        for (path, file) in &self.disk.files {
            if !scope.covers(path) || walked.files.contains_key(path) {
                continue;
            }
            if walked.unreadable.keys().any(|dir| within(path, dir)) {
                continue;
            }
            if rules.excluded(path, false) {
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

    /// Merges entries a peer sent. Returns the paths that changed.
    pub fn merge(&mut self, entries: &[Entry]) -> Vec<String> {
        let mut changed = Vec::new();
        for entry in entries {
            changed.extend(self.index.merge(entry));
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

    pub fn adopted(&mut self, path: &str, version: Version) {
        if let Some(mut file) = self.disk.get(path).cloned() {
            file.version = version;
            self.disk.insert(path.to_owned(), file);
        }
    }

    pub fn placed(&mut self, path: &str, meta: FileMeta, stat: Stat, version: Version) {
        self.disk.insert(
            path.to_owned(),
            LocalFile {
                meta,
                stat,
                version,
            },
        );
    }

    pub fn removed(&mut self, path: &str) {
        self.disk.remove(path);
    }

    /// The conflict copies in the index that still exist.
    pub fn conflicts(&self) -> Vec<String> {
        self.index
            .entries()
            .filter(|entry| entry.file.is_some() && crate::folder::index::is_conflict(&entry.path))
            .map(|entry| entry.path.clone())
            .collect()
    }

    /// Files and bytes in the index, deleted paths aside.
    pub fn totals(&self) -> (u64, u64) {
        self.index
            .entries()
            .filter_map(|entry| entry.file.as_ref())
            .fold((0, 0), |(files, bytes), file| {
                (files + 1, bytes + file.size)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::folder::{disk, index::Kind, scan};
    use std::fs;

    /// Scans `root` fully, reading files as the folder would.
    fn scan(state: &mut State, root: &std::path::Path) {
        let rules = Rules::new(root, true);
        let walked = scan::walk(root, &rules, &Scope::All);
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

        scan(&mut state, root);
        assert_eq!(content(&state, "a"), Some(Hash::of(b"one")));
        let seq = state.index.seq();

        scan(&mut state, root);
        assert_eq!(
            state.index.seq(),
            seq,
            "an unchanged folder records nothing"
        );

        fs::write(root.join("a"), "uno").expect("write");
        fs::remove_file(root.join("b")).expect("remove");
        scan(&mut state, root);
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
        scan(&mut state, root);
        assert!(state.disk.get("build.log").is_some());

        fs::write(root.join(".gitignore"), "*.log\n").expect("write");
        scan(&mut state, root);
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
    fn work_follows_the_index() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path();
        fs::write(root.join("keep"), "same").expect("write");
        fs::write(root.join("drop"), "bye").expect("write");
        let mut state = State::new(1);
        scan(&mut state, root);
        let rules = Rules::new(root, true);

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
        scan(&mut state, root);

        assert_eq!(state.index.seq(), seq, "no edit for content the index has");
        assert_eq!(
            state.disk.get("same").map(|file| file.version.clone()),
            Some(theirs.version)
        );
    }

    #[test]
    fn state_survives_a_round_trip() {
        let dir = tempfile::tempdir().expect("a temp dir");
        fs::write(dir.path().join("a"), "one").expect("write");
        let mut state = State::new(1);
        scan(&mut state, dir.path());

        let loaded = State::decode(&state.encode().expect("encodes")).expect("decodes");
        assert_eq!(loaded.disk.files(), state.disk.files());
        assert!(loaded.index.seq() > state.index.seq());
        assert!(loaded.pending().contains("a"));
    }

    #[test]
    fn the_disk_finds_content_and_case_clashes() {
        let mut disk = Disk::default();
        let file = |byte: u8| LocalFile {
            meta: FileMeta {
                kind: Kind::File,
                hash: Hash([byte; 32]),
                size: 1,
                mtime_ms: 0,
                executable: false,
            },
            stat: Stat {
                kind: Kind::File,
                size: 1,
                mtime_ns: 0,
                inode: 0,
                executable: false,
            },
            version: Version::default(),
        };
        disk.insert("Readme.md".into(), file(1));
        disk.insert("b".into(), file(1));

        assert_eq!(
            disk.holding(&Hash([1; 32]), "b").map(|(path, _)| path),
            Some("b")
        );
        assert_eq!(disk.case_clash("README.md"), Some("Readme.md"));
        assert_eq!(disk.case_clash("Readme.md"), None);
        disk.remove("b");
        assert_eq!(
            disk.holding(&Hash([1; 32]), "b").map(|(path, _)| path),
            Some("Readme.md")
        );
    }
}
