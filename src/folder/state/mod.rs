//! What a device keeps about one folder: the index (where the folder should
//! be), the disk record (where it is), the Git memory and the peers' cursors.
//! The logic here is synchronous and does no I/O beyond what callers pass
//! in, so it can be tested on its own. See DESIGN.md, "What is on disk".

mod awaiting;
mod on_disk;
mod scan;
mod work;

pub use on_disk::{Disk, LocalFile};
pub use scan::{Gone, ScanPlan, ToRead};
pub use work::Work;

use crate::{
    folder::{
        disk::Stat,
        index::{Entry, FileMeta, Hash, Index, Replica, Version},
    },
    git,
    ids::DeviceId,
};
use awaiting::Awaiting;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

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
    /// Paths whose content is being fetched. They sit out apply rounds until
    /// it arrives, so a round costs what changed, not everything still
    /// downloading.
    awaiting: Awaiting,
}

impl State {
    pub fn new(replica: Replica) -> Self {
        Self {
            index: Index::new(replica),
            disk: Disk::default(),
            cursors: BTreeMap::new(),
            git: git::Memory::default(),
            pending: BTreeSet::new(),
            awaiting: Awaiting::default(),
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
            disk: self.disk.files(),
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
            awaiting: Awaiting::default(),
        };
        state.pend_all();
        Ok(state)
    }

    /// Forgets what was on disk, so a fresh scan deletes nothing. The index,
    /// where the folder should be, stays.
    pub fn forget_disk(&mut self) {
        self.disk.clear();
        self.pend_all();
    }

    /// Marks every path for a look, as when the rules changed and paths
    /// they left out may sync now.
    pub fn pend_all(&mut self) {
        self.awaiting = Awaiting::default();
        self.pending = self
            .index
            .entries()
            .map(|entry| entry.path.clone())
            .chain(self.disk.files().keys().cloned())
            .collect();
    }

    pub fn pending(&self) -> &BTreeSet<String> {
        &self.pending
    }

    pub fn pend(&mut self, paths: impl IntoIterator<Item = String>) {
        for path in paths {
            self.awaiting.remove(&path);
            self.pending.insert(path);
        }
    }

    /// Moves `path` from the pending list to wait for content `hash`, if
    /// its index entry is still at `planned`.
    pub fn await_content(&mut self, path: &str, hash: Hash, planned: &Version) {
        if self.index.get(path).map(|entry| &entry.version) == Some(planned) {
            self.pending.remove(path);
            self.awaiting.insert(path.to_owned(), hash);
        }
    }

    /// Puts the paths waiting for content `hash` back on the pending list:
    /// it arrived, or a file here holds it now.
    pub fn content_arrived(&mut self, hash: &Hash) {
        let paths = self.awaiting.take(hash);
        self.pending.extend(paths);
    }

    /// The paths waiting for content, with the content each waits for.
    pub fn awaiting(&self) -> impl Iterator<Item = (&str, &Hash)> {
        self.awaiting.paths()
    }

    /// The paths waiting for content `hash`.
    pub fn awaiting_for(&self, hash: &Hash) -> impl Iterator<Item = &str> {
        self.awaiting.waiting_for(hash)
    }

    /// Takes `path` off the pending list if its index entry is still at
    /// `planned`, the version the finished work was planned for. An entry
    /// that moved meanwhile keeps the path pending.
    pub fn settle(&mut self, path: &str, planned: Option<&Version>) {
        if self.index.get(path).map(|entry| &entry.version) == planned {
            self.pending.remove(path);
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

    pub fn adopted(&mut self, path: &str, version: Version) {
        if let Some(mut file) = self.disk.get(path).cloned() {
            file.version = version;
            self.disk.insert(path.to_owned(), file);
        }
    }

    pub fn placed(&mut self, path: &str, meta: FileMeta, stat: Stat, version: Version) {
        self.content_arrived(&meta.hash);
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
    use super::{scan::tests::scan_all, *};
    use crate::folder::index::Kind;
    use std::fs;

    #[test]
    fn paths_wait_for_content_outside_the_pending_list() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut state = State::new(1);
        let hash = Hash::of(b"far away");
        let theirs = Entry {
            path: "a".into(),
            version: Version::default().with(2, 1),
            file: Some(FileMeta {
                kind: Kind::File,
                hash,
                size: 8,
                mtime_ms: 0,
                executable: false,
            }),
        };
        state.merge(std::slice::from_ref(&theirs));

        state.await_content("a", hash, &theirs.version);
        assert!(!state.pending().contains("a"));
        assert_eq!(state.awaiting_for(&hash).collect::<Vec<_>>(), ["a"]);

        state.content_arrived(&hash);
        assert!(state.pending().contains("a"));
        assert_eq!(state.awaiting().count(), 0);

        state.await_content("a", hash, &theirs.version);
        fs::write(dir.path().join("copy"), "far away").expect("write");
        scan_all(&mut state, dir.path());
        assert!(
            state.pending().contains("a"),
            "a file here with the content wakes it"
        );

        state.await_content("a", hash, &Version::default());
        assert!(
            state.pending().contains("a"),
            "an entry that moved since the work was planned stays pending"
        );
    }

    #[test]
    fn state_survives_a_round_trip() {
        let dir = tempfile::tempdir().expect("a temp dir");
        fs::write(dir.path().join("a"), "one").expect("write");
        let mut state = State::new(1);
        scan_all(&mut state, dir.path());

        let loaded = State::decode(&state.encode().expect("encodes")).expect("decodes");
        assert_eq!(loaded.disk.files(), state.disk.files());
        assert!(loaded.index.seq() > state.index.seq());
        assert!(loaded.pending().contains("a"));
    }
}
