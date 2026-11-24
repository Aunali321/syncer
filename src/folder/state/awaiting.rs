//! Paths waiting for content being fetched.

use super::on_disk::drop_from;
use crate::folder::index::Hash;
use std::collections::{BTreeSet, HashMap};

/// Paths waiting for content, by path and by the content they wait for.
#[derive(Debug, Default)]
pub(super) struct Awaiting {
    by_path: HashMap<String, Hash>,
    by_hash: HashMap<Hash, BTreeSet<String>>,
}

impl Awaiting {
    pub(super) fn insert(&mut self, path: String, hash: Hash) {
        self.remove(&path);
        self.by_hash.entry(hash).or_default().insert(path.clone());
        self.by_path.insert(path, hash);
    }

    pub(super) fn remove(&mut self, path: &str) {
        if let Some(hash) = self.by_path.remove(path) {
            drop_from(&mut self.by_hash, &hash, path);
        }
    }

    /// Takes every path waiting for `hash`.
    pub(super) fn take(&mut self, hash: &Hash) -> BTreeSet<String> {
        let paths = self.by_hash.remove(hash).unwrap_or_default();
        for path in &paths {
            self.by_path.remove(path);
        }
        paths
    }

    /// Every waiting path, with the content it waits for.
    pub(super) fn paths(&self) -> impl Iterator<Item = (&str, &Hash)> {
        self.by_path
            .iter()
            .map(|(path, hash)| (path.as_str(), hash))
    }

    /// The paths waiting for content `hash`.
    pub(super) fn waiting_for(&self, hash: &Hash) -> impl Iterator<Item = &str> {
        self.by_hash
            .get(hash)
            .into_iter()
            .flatten()
            .map(String::as_str)
    }
}
