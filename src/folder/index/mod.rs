//! A folder's index: for every path, the newest version this device knows and
//! the file it names, or a tombstone. Indexes on different devices converge
//! whatever order entries arrive in. See DESIGN.md, "The index".

mod entry;
mod names;

pub use entry::{
    Entry, FileMeta, Hash, Kind, Merge, Order, Replica, Version, conflict_path, is_conflict, merge,
    original_of,
};
pub use names::{is_git_name, path_problem, windows_refuses};

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Stored {
    entry: Entry,
    /// The index's sequence number when this entry last changed.
    seq: u64,
}

/// Larger than the number of changes a device makes between two saves of its
/// index. See `Index::loaded`.
const LOAD_GAP: u64 = 1 << 32;

/// The most entries one `since` call returns.
pub const BATCH: usize = 1000;

/// One device's index of one folder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Index {
    replica: Replica,
    /// The highest count this replica gave an edit, or saw for itself.
    clock: u64,
    /// The last change's sequence number.
    seq: u64,
    entries: BTreeMap<String, Stored>,
    /// Paths by the sequence number of their last change.
    #[serde(skip)]
    by_seq: BTreeMap<u64, String>,
}

impl Index {
    pub fn new(replica: Replica) -> Self {
        Self {
            replica,
            clock: 0,
            seq: 0,
            entries: BTreeMap::new(),
            by_seq: BTreeMap::new(),
        }
    }

    /// Rebuilds what is not saved, after loading. A crash can load an index
    /// older than what peers already received from it. Moving the counters
    /// past any given out before keeps a peer's cursor from skipping the next
    /// changes, and an old edit's version from being reused for a new one.
    pub fn loaded(mut self) -> Self {
        self.seq += LOAD_GAP;
        self.clock += LOAD_GAP;
        self.by_seq = self
            .entries
            .iter()
            .map(|(path, stored)| (stored.seq, path.clone()))
            .collect();
        self
    }

    pub fn replica(&self) -> Replica {
        self.replica
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, path: &str) -> Option<&Entry> {
        self.entries.get(path).map(|stored| &stored.entry)
    }

    /// Whether `path` names a file or link that was not deleted.
    pub fn is_live(&self, path: &str) -> bool {
        self.get(path).is_some_and(|entry| entry.file.is_some())
    }

    /// Whether any path inside the directory `dir` is live.
    pub fn live_under(&self, dir: &str) -> bool {
        let prefix = format!("{dir}/");
        self.entries
            .range(prefix.clone()..)
            .take_while(|(path, _)| path.starts_with(&prefix))
            .any(|(_, stored)| stored.entry.file.is_some())
    }

    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries.values().map(|stored| &stored.entry)
    }

    fn put(&mut self, entry: Entry) {
        self.seq += 1;
        let seq = self.seq;
        let path = entry.path.clone();

        if let Some(old) = self.entries.insert(path.clone(), Stored { entry, seq }) {
            self.by_seq.remove(&old.seq);
        }

        self.by_seq.insert(seq, path);
    }

    /// Merges an entry another device sent. Returns the paths that changed,
    /// the conflict copy's first.
    pub fn merge(&mut self, incoming: &Entry) -> Vec<String> {
        // This replica's own counts, seen again after a crash, are never reused.
        self.clock = self.clock.max(incoming.version.get(self.replica));
        let merged = merge(self.get(&incoming.path), incoming);
        let mut changed = Vec::new();

        // The copy first: a peer reading changes in order never sees the path
        // settled before it knows where the losing edit went.
        if let Some(copy) = merged.copy {
            changed.extend(self.merge(&copy));
        }

        if let Some(entry) = merged.entry {
            changed.push(entry.path.clone());
            self.put(entry);
        }

        changed
    }

    /// Records this device's own edit of `path`, made to the content of
    /// version `base`; `file` is `None` for a deletion. Returns the edit's
    /// version and the paths that changed (more than one when the edit is
    /// concurrent with an entry that arrived in the meantime).
    pub fn edit(
        &mut self,
        path: &str,
        base: &Version,
        file: Option<FileMeta>,
    ) -> (Version, Vec<String>) {
        self.clock += 1;
        let entry = Entry {
            path: path.to_owned(),
            version: base.with(self.replica, self.clock),
            file,
        };
        let changed = self.merge(&entry);
        (entry.version, changed)
    }

    /// The entries changed after `seq`, oldest change first, at most `limit`,
    /// and the sequence number of the last one given.
    pub fn since(&self, seq: u64, limit: usize) -> (Vec<Entry>, u64) {
        let mut entries = Vec::new();
        let mut upto = seq;

        for (&at, path) in self.by_seq.range(seq + 1..).take(limit) {
            if let Some(stored) = self.entries.get(path) {
                entries.push(stored.entry.clone());
            }
            upto = at;
        }

        (entries, upto)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(byte: u8, mtime_ms: i64) -> FileMeta {
        FileMeta {
            kind: Kind::File,
            hash: Hash([byte; 32]),
            size: 1,
            mtime_ms,
            executable: false,
        }
    }

    fn entry(path: &str, version: &[(Replica, u64)], file: Option<FileMeta>) -> Entry {
        Entry {
            path: path.into(),
            version: version
                .iter()
                .fold(Version::default(), |at, &(replica, count)| {
                    at.with(replica, count)
                }),
            file,
        }
    }

    #[test]
    fn indexes_converge_in_any_order() {
        let edits = [
            entry("x", &[(1, 1)], Some(file(1, 10))),
            entry("x", &[(2, 1)], Some(file(2, 30))),
            entry("x", &[(3, 1)], Some(file(3, 20))),
            entry("y", &[(2, 2)], Some(file(4, 1))),
        ];
        // A fourth replica that saw all of it deletes `x`: what it sends is its
        // whole index, conflict copies included.
        let mut deleter = Index::new(4);
        for edit in &edits {
            deleter.merge(edit);
        }
        let base = deleter
            .get("x")
            .map(|e| e.version.clone())
            .unwrap_or_default();
        deleter.edit("x", &base, None);
        let mut sent: Vec<Entry> = edits.to_vec();
        sent.extend(deleter.since(0, usize::MAX).0);

        let mut results = Vec::new();
        for order in permutations(sent.len()).into_iter().step_by(97) {
            let mut index = Index::new(99);
            for &i in &order {
                index.merge(&sent[i]);
            }
            let mut all: Vec<Entry> = index.entries().cloned().collect();
            all.sort_by(|a, b| a.path.cmp(&b.path));
            results.push(all);
        }

        for pair in results.windows(2) {
            assert_eq!(pair[0], pair[1]);
        }
        let last = results.last().expect("results");
        assert_eq!(last.iter().filter(|e| is_conflict(&e.path)).count(), 2);
        assert!(last.iter().any(|e| e.path == "x" && e.file.is_none()));
    }

    #[test]
    fn replicas_that_gossip_end_equal() {
        let mut replicas: Vec<Index> = (1..=4).map(Index::new).collect();
        let mut seed = 7u64;
        let mut next = |bound: u64| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) % bound
        };

        for round in 0..200 {
            let at = next(4) as usize;
            let path = ["a", "b", "c"][next(3) as usize];
            let base = replicas[at]
                .get(path)
                .map(|e| e.version.clone())
                .unwrap_or_default();
            let file = (next(5) > 0).then(|| file(next(250) as u8, round));
            replicas[at].edit(path, &base, file);

            let (from, to) = (next(4) as usize, next(4) as usize);
            if from != to {
                let (sent, _) = replicas[from].since(0, usize::MAX);
                for entry in &sent {
                    replicas[to].merge(entry);
                }
            }
        }

        // Everyone hears everything until nothing changes.
        loop {
            let mut changed = false;
            for from in 0..4 {
                let (sent, _) = replicas[from].since(0, usize::MAX);
                for replica in &mut replicas {
                    for entry in &sent {
                        changed |= !replica.merge(entry).is_empty();
                    }
                }
            }
            if !changed {
                break;
            }
        }

        let views: Vec<Vec<Entry>> = replicas
            .iter()
            .map(|r| r.entries().cloned().collect())
            .collect();
        for pair in views.windows(2) {
            assert_eq!(pair[0], pair[1]);
        }
    }

    fn permutations(n: usize) -> Vec<Vec<usize>> {
        if n == 0 {
            return vec![vec![]];
        }
        let mut out = Vec::new();
        for rest in permutations(n - 1) {
            for at in 0..=rest.len() {
                let mut order = rest.clone();
                order.insert(at, n - 1);
                out.push(order);
            }
        }
        out
    }

    #[test]
    fn an_edit_of_old_content_conflicts_with_a_newer_arrival() {
        let mut index = Index::new(1);
        let (base, _) = index.edit("a", &Version::default(), Some(file(1, 10)));
        // A peer's newer version arrives but is not on disk yet.
        index.merge(&entry("a", &[(1, 1), (2, 1)], Some(file(2, 20))));
        // The user edits the old content.
        let (_, changed) = index.edit("a", &base, Some(file(3, 30)));

        assert_eq!(
            changed,
            vec!["a (conflict 02020202)".to_owned(), "a".to_owned()]
        );
        assert_eq!(
            index.get("a").and_then(|e| e.file.clone()),
            Some(file(3, 30))
        );
    }

    #[test]
    fn since_gives_changes_in_order_and_a_cursor() {
        let mut index = Index::new(1);
        index.edit("a", &Version::default(), Some(file(1, 1)));
        index.edit("b", &Version::default(), Some(file(2, 1)));
        let base = index
            .get("a")
            .map(|e| e.version.clone())
            .unwrap_or_default();
        index.edit("a", &base, Some(file(3, 1)));

        let (entries, upto) = index.since(0, 10);
        let paths: Vec<_> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["b", "a"]);
        assert_eq!(upto, index.seq());

        let (first, cursor) = index.since(0, 1);
        assert_eq!(first.len(), 1);
        assert_eq!(index.since(cursor, 10).0.len(), 1);
    }

    #[test]
    fn a_loaded_index_never_reuses_numbers() {
        let mut index = Index::new(1);
        index.edit("a", &Version::default(), Some(file(1, 1)));
        let bytes = rmp_serde::to_vec(&index).expect("encodes");
        let loaded: Index = rmp_serde::from_slice(&bytes).expect("decodes");
        let loaded = loaded.loaded();

        assert!(loaded.seq() > index.seq() + 1_000_000);
        assert_eq!(loaded.since(0, 10).0.len(), 1);
    }
}
