//! The record of what is on disk, with lookups by content and by the name
//! the file system would find a path under.

use crate::folder::{
    disk::{Folding, Stat},
    index::{FileMeta, Hash, Version},
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
/// locally) and by folded path (to catch names the file system would take
/// for one another).
#[derive(Debug, Default)]
pub struct Disk {
    folding: Folding,
    files: BTreeMap<String, LocalFile>,
    by_hash: HashMap<Hash, BTreeSet<String>>,
    by_folded: HashMap<String, BTreeSet<String>>,
}

impl Disk {
    pub(super) fn from_files(files: BTreeMap<String, LocalFile>) -> Self {
        let mut disk = Self::default();
        for (path, file) in files {
            disk.insert(path, file);
        }
        disk
    }

    /// Sets how the file system compares names, as probed.
    pub fn set_folding(&mut self, folding: Folding) {
        let files = std::mem::take(&mut self.files);
        *self = Self {
            folding,
            ..Self::default()
        };
        for (path, file) in files {
            self.insert(path, file);
        }
    }

    pub fn folding(&self) -> Folding {
        self.folding
    }

    /// Forgets every file, keeping the folding.
    pub(super) fn clear(&mut self) {
        *self = Self {
            folding: self.folding,
            ..Self::default()
        };
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
        self.by_folded
            .entry(self.folding.key(&path))
            .or_default()
            .insert(path.clone());
        self.files.insert(path, file);
    }

    pub fn remove(&mut self, path: &str) -> Option<LocalFile> {
        let file = self.files.remove(path)?;
        drop_from(&mut self.by_hash, &file.meta.hash, path);
        drop_from(&mut self.by_folded, &self.folding.key(path), path);
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

    /// Another path on disk that the file system would take for `path`.
    pub fn clash(&self, path: &str) -> Option<&str> {
        self.by_folded
            .get(&self.folding.key(path))?
            .iter()
            .find(|other| *other != path)
            .map(String::as_str)
    }
}

pub(super) fn drop_from<K: std::hash::Hash + Eq>(
    map: &mut HashMap<K, BTreeSet<String>>,
    key: &K,
    path: &str,
) {
    if let Some(paths) = map.get_mut(key) {
        paths.remove(path);
        if paths.is_empty() {
            map.remove(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::folder::index::Kind;

    #[test]
    fn the_disk_finds_content_and_clashing_names() {
        let mut disk = Disk::default();
        disk.set_folding(Folding {
            case: true,
            trailing: true,
            unicode: true,
        });
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
                ctime_ns: 0,
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
        disk.insert("caf\u{e9}".into(), file(2));
        assert_eq!(disk.clash("README.md"), Some("Readme.md"));
        assert_eq!(disk.clash("Readme.md"), None);
        assert_eq!(disk.clash("README.md."), Some("Readme.md"));
        assert_eq!(disk.clash("CAFE\u{301}"), Some("caf\u{e9}"));
        disk.remove("b");
        assert_eq!(
            disk.holding(&Hash([1; 32]), "b").map(|(path, _)| path),
            Some("Readme.md")
        );
    }
}
