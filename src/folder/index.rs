//! A folder's index: for every path, the newest version this device knows and
//! the file it names, or a tombstone. Indexes on different devices converge
//! whatever order entries arrive in. See DESIGN.md, "The index".

use serde::{Deserialize, Serialize};
use std::{cmp::Ordering, collections::BTreeMap, fmt};

/// One device's copy of one folder's index.
pub type Replica = u64;

/// How far a version is from another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    Same,
    /// It has seen every edit the other has, and more.
    Newer,
    Older,
    Concurrent,
}

/// Edits per replica.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Version(BTreeMap<Replica, u64>);

impl Version {
    pub fn get(&self, replica: Replica) -> u64 {
        self.0.get(&replica).copied().unwrap_or(0)
    }

    pub fn compare(&self, other: &Self) -> Order {
        let (mut ahead, mut behind) = (false, false);

        for replica in self.0.keys().chain(other.0.keys()) {
            match self.get(*replica).cmp(&other.get(*replica)) {
                Ordering::Greater => ahead = true,
                Ordering::Less => behind = true,
                Ordering::Equal => {}
            }
        }

        match (ahead, behind) {
            (false, false) => Order::Same,
            (true, false) => Order::Newer,
            (false, true) => Order::Older,
            (true, true) => Order::Concurrent,
        }
    }

    /// Every edit either has seen.
    pub fn union(&self, other: &Self) -> Self {
        let mut out = self.clone();

        for (&replica, &count) in &other.0 {
            let mine = out.0.entry(replica).or_default();
            *mine = (*mine).max(count);
        }

        out
    }

    /// This version with `replica`'s edit `count` on top.
    pub fn with(&self, replica: Replica, count: u64) -> Self {
        let mut out = self.clone();
        out.0.insert(replica, count);
        out
    }
}

/// A blake3 content hash.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Hash(#[serde(with = "serde_bytes")] pub [u8; 32]);

impl Hash {
    pub fn of(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    /// The first eight hex digits, for names and logs.
    pub fn short(&self) -> String {
        hex::encode(&self.0[..4])
    }
}

impl From<blake3::Hash> for Hash {
    fn from(hash: blake3::Hash) -> Self {
        Self(*hash.as_bytes())
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Kind {
    File,
    /// The content is the link's target.
    Symlink,
}

/// A file's content as the index holds it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileMeta {
    pub kind: Kind,
    pub hash: Hash,
    pub size: u64,
    /// Milliseconds since the Unix epoch.
    pub mtime_ms: i64,
    pub executable: bool,
}

impl FileMeta {
    /// Whether `other` has the same content and mode. The mtime is not
    /// content: a file touched without change is the same file.
    pub fn same_content(&self, other: &Self) -> bool {
        self.kind == other.kind && self.hash == other.hash && self.executable == other.executable
    }
}

/// One path in the index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Relative to the folder root, parts joined with `/`.
    pub path: String,
    pub version: Version,
    /// `None` for a deleted path.
    pub file: Option<FileMeta>,
}

/// What merging one incoming entry decides.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Merge {
    /// The path's new entry, or `None` when the index keeps its own.
    pub entry: Option<Entry>,
    /// The conflict copy the losing side of two concurrent edits becomes.
    pub copy: Option<Entry>,
}

/// Which of two concurrent entries keeps the path: a file before a
/// tombstone, then the later mtime, then the larger hash, then the
/// executable one. No two different files tie, so every device picks the
/// same winner.
fn rank(entry: &Entry) -> (bool, i64, Hash, bool) {
    match &entry.file {
        Some(file) => (true, file.mtime_ms, file.hash, file.executable),
        None => (false, 0, Hash([0; 32]), false),
    }
}

/// Merges `incoming` into `local`, the entry the index has for its path.
/// Concurrent edits are both kept: the winner by `rank` keeps the path with
/// both versions' edits, and the loser moves to a conflict copy: a new file
/// whose name and version every device derives the same way.
pub fn merge(local: Option<&Entry>, incoming: &Entry) -> Merge {
    let Some(local) = local else {
        return Merge {
            entry: Some(incoming.clone()),
            copy: None,
        };
    };

    match local.version.compare(&incoming.version) {
        Order::Newer => return Merge::default(),
        Order::Same if local == incoming => return Merge::default(),
        Order::Older => {
            return Merge {
                entry: Some(incoming.clone()),
                copy: None,
            };
        }
        // Concurrent, or the same version with other content (which only a
        // broken peer sends): settled the same way everywhere.
        Order::Same | Order::Concurrent => {}
    }

    let (winner, loser) = if rank(incoming) > rank(local) {
        (incoming, local)
    } else {
        (local, incoming)
    };
    let entry = Entry {
        path: local.path.clone(),
        version: local.version.union(&incoming.version),
        file: winner.file.clone(),
    };
    let copy = match (&loser.file, &winner.file) {
        (Some(lost), Some(won)) if lost.same_content(won) => None,
        // A new file nobody edited yet: the empty version, so every device
        // derives the same entry whatever order it merged in, and any edit
        // or deletion of the copy is newer.
        (Some(lost), _) => Some(Entry {
            path: conflict_path(&local.path, &lost.hash),
            version: Version::default(),
            file: Some(lost.clone()),
        }),
        (None, _) => None,
    };

    Merge {
        entry: (entry != *local).then_some(entry),
        copy,
    }
}

const CONFLICT_MARK: &str = " (conflict ";

/// Where a losing edit of `path` with content `hash` goes:
/// `notes (conflict 1a2b3c4d).txt` beside `notes.txt`.
pub fn conflict_path(path: &str, hash: &Hash) -> String {
    let tag = hash.short();
    let name_at = path.rfind('/').map_or(0, |at| at + 1);

    match path[name_at..].rfind('.').filter(|&dot| dot > 0) {
        Some(dot) => {
            let dot = name_at + dot;
            format!("{}{CONFLICT_MARK}{tag}){}", &path[..dot], &path[dot..])
        }
        None => format!("{path}{CONFLICT_MARK}{tag})"),
    }
}

/// The path a conflict copy stands beside, or `None` when `path` is not one.
pub fn original_of(path: &str) -> Option<String> {
    let name_at = path.rfind('/').map_or(0, |at| at + 1);
    let at = name_at + path[name_at..].rfind(CONFLICT_MARK)?;
    let tag_at = at + CONFLICT_MARK.len();
    let tag = path.get(tag_at..tag_at + 8)?;

    if !tag.bytes().all(|b| b.is_ascii_hexdigit()) || path.get(tag_at + 8..tag_at + 9)? != ")" {
        return None;
    }

    let original = format!("{}{}", &path[..at], &path[tag_at + 9..]);
    (original.len() > name_at).then_some(original)
}

pub fn is_conflict(path: &str) -> bool {
    original_of(path).is_some()
}

/// The longest path a peer may name.
const MAX_PATH: usize = 4096;

/// Whether `part` names a `.git` directory on some file system: `.git` in
/// any case, with the trailing dots and spaces Windows drops, or its short
/// name.
pub fn is_git_name(part: &str) -> bool {
    let trimmed = part.trim_end_matches(['.', ' ']);
    trimmed.eq_ignore_ascii_case(".git") || part.eq_ignore_ascii_case("git~1")
}

/// Whether Windows refuses `part` as a file name or reads it as something
/// else: a device name, a stream, a name ending in a dot or space.
fn windows_refuses(part: &str) -> bool {
    const DEVICES: [&str; 22] = [
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
        "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
    ];
    let stem = part
        .split('.')
        .next()
        .unwrap_or(part)
        .trim_end()
        .to_ascii_lowercase();
    part.contains([':', '<', '>', '"', '|', '?', '*'])
        || part.ends_with(['.', ' '])
        || DEVICES.contains(&stem.as_str())
}

/// Whether a peer may name `path`: plain names joined with `/`, none of them
/// `.git` in any spelling, and not syncer's own folder at the root. On
/// Windows, also no name Windows would read as something else.
pub fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= MAX_PATH
        && !path
            .split('/')
            .next()
            .is_some_and(|first| first.eq_ignore_ascii_case(crate::folder::OWN_DIR))
        && path.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && !part.contains(['\\', '\0'])
                && !part.chars().any(char::is_control)
                && !is_git_name(part)
                && !(cfg!(windows) && windows_refuses(part))
        })
}

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
            version: Version(version.iter().copied().collect()),
            file,
        }
    }

    #[test]
    fn versions_compare_by_every_replica() {
        let a = Version([(1, 2), (2, 1)].into());
        let b = Version([(1, 2)].into());
        let c = Version([(1, 1), (2, 2)].into());

        assert_eq!(a.compare(&a), Order::Same);
        assert_eq!(a.compare(&b), Order::Newer);
        assert_eq!(b.compare(&a), Order::Older);
        assert_eq!(a.compare(&c), Order::Concurrent);
        assert_eq!(a.union(&c), Version([(1, 2), (2, 2)].into()));
    }

    #[test]
    fn a_newer_entry_replaces_and_an_older_one_is_ignored() {
        let old = entry("a", &[(1, 1)], Some(file(1, 10)));
        let new = entry("a", &[(1, 2)], Some(file(2, 5)));

        assert_eq!(merge(Some(&old), &new).entry, Some(new.clone()));
        assert_eq!(merge(Some(&new), &old), Merge::default());
    }

    #[test]
    fn concurrent_edits_keep_the_later_and_copy_the_other() {
        let mine = entry("dir/notes.txt", &[(1, 1)], Some(file(1, 10)));
        let theirs = entry("dir/notes.txt", &[(2, 1)], Some(file(2, 20)));

        let merged = merge(Some(&mine), &theirs);
        let kept = merged.entry.expect("the path changes");
        let copy = merged.copy.expect("a conflict copy");

        assert_eq!(kept.file, theirs.file);
        assert_eq!(kept.version, Version([(1, 1), (2, 1)].into()));
        assert_eq!(copy.path, "dir/notes (conflict 01010101).txt");
        assert_eq!(copy.file, mine.file);
        assert_eq!(copy.version, Version::default());
    }

    #[test]
    fn an_edit_beats_a_concurrent_delete_without_a_copy() {
        let edit = entry("a", &[(1, 1)], Some(file(1, 10)));
        let delete = entry("a", &[(2, 1)], None);

        let merged = merge(Some(&delete), &edit);
        assert_eq!(merged.entry.and_then(|e| e.file), edit.file);
        assert_eq!(merged.copy, None);
        assert_eq!(merge(Some(&edit), &delete).copy, None);
    }

    #[test]
    fn concurrent_identical_content_makes_no_copy() {
        let a = entry("a", &[(1, 1)], Some(file(7, 10)));
        let b = entry("a", &[(2, 1)], Some(file(7, 20)));

        assert_eq!(merge(Some(&a), &b).copy, None);
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

    #[test]
    fn conflict_names_round_trip() {
        let hash = Hash([0xab; 32]);
        for path in ["a.txt", "dir/a.tar.gz", "Makefile", "dir/.env", ".env"] {
            let copy = conflict_path(path, &hash);
            assert!(is_conflict(&copy), "{copy}");
            assert_eq!(original_of(&copy).as_deref(), Some(path));
        }
        assert_eq!(
            conflict_path("dir/.env", &hash),
            "dir/.env (conflict abababab)"
        );
        assert!(!is_conflict("notes (conflict of interest).txt"));
    }

    #[test]
    fn paths_from_peers_are_checked() {
        assert!(valid_path("src/main.rs"));
        assert!(valid_path("a/.gitignore"));
        assert!(
            windows_refuses("aux.c") && windows_refuses("a:b") && !windows_refuses("auxiliary")
        );
        for bad in [
            "",
            "/a",
            "a//b",
            "a/../b",
            "./a",
            "a/.git/config",
            ".GIT",
            "a\\b",
            ".syncer/folder",
            ".git./hooks/post-checkout",
            "a/.Git /config",
            "GIT~1/config",
            "a\u{7}b",
        ] {
            assert!(!valid_path(bad), "{bad}");
        }
        assert!(windows_refuses("aux.c") && windows_refuses("a:b") && windows_refuses("x."));
        assert!(!windows_refuses("auxiliary.rs") && !windows_refuses(".gitignore"));
    }
}
