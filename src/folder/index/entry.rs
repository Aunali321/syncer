//! One path's entry: its version, its content, and how two concurrent
//! entries settle into one, the same way on every device, the losing one
//! as a conflict copy.

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
}
