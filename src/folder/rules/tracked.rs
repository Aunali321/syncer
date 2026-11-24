//! What the checked-out commit tracks, which Git's ignore files never leave
//! out.

use crate::git::Oid;
use std::collections::BTreeSet;

/// The paths the checked-out commit holds. Git's ignore files never leave a
/// tracked path out, just as `git status` still shows it.
#[derive(Debug, Default)]
pub struct Tracked {
    head: Option<Oid>,
    paths: BTreeSet<String>,
}

impl Tracked {
    pub fn new(head: Option<Oid>, paths: BTreeSet<String>) -> Self {
        Self { head, paths }
    }

    /// The commit these paths are from.
    pub fn head(&self) -> Option<&Oid> {
        self.head.as_ref()
    }

    /// Whether `path` is tracked, or for a directory, holds a tracked path.
    pub fn covers(&self, path: &str, is_dir: bool) -> bool {
        if !is_dir {
            return self.paths.contains(path);
        }
        let prefix = format!("{path}/");
        self.paths
            .range(prefix.clone()..)
            .next()
            .is_some_and(|tracked| tracked.starts_with(&prefix))
    }
}
