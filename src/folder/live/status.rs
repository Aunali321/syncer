//! Where a folder stands, for `syncer status`.

use crate::{
    folder::state::Work,
    git::{self},
    ids::DeviceId,
    util::lock,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::*;

/// Where a folder stands, for `syncer status`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Problems {
    /// Why the whole folder is stopped, if it is.
    pub folder: Option<Stopped>,
    /// Paths that could not be brought in line, with why.
    pub files: BTreeMap<String, String>,
    /// Paths here whose names cannot sync to any device, with why.
    pub names: BTreeMap<String, String>,
    /// Git sync with each peer: diverged branches, operations waited for.
    pub git: BTreeMap<DeviceId, GitNotes>,
    /// Peers whose changes are held back, with why.
    pub waiting: BTreeMap<DeviceId, String>,
}

/// The folder's directory is gone, or holds no marker: an unmounted disk or
/// a replaced directory. Nothing is touched until `syncer reset`.
#[derive(Debug, thiserror::Error)]
#[error("folder not found")]
pub struct Missing;

/// Why a folder is stopped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stopped {
    Missing,
    Other(String),
}

impl From<&anyhow::Error> for Stopped {
    fn from(error: &anyhow::Error) -> Self {
        if error.is::<Missing>() {
            Self::Missing
        } else {
            Self::Other(crate::util::describe(error))
        }
    }
}

/// Git sync with one peer, for `syncer status`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GitNotes {
    pub diverged: Vec<Split>,
    /// Anything else to say: operations waited for, names ignored.
    pub notes: Vec<String>,
}

/// A branch both devices committed to while apart.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Split {
    pub branch: String,
    /// What `git merge` takes to bring the peer's side in.
    pub merge: String,
}

/// Counts for `syncer status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub files: u64,
    pub bytes: u64,
    pub pending: usize,
    pub conflicts: Vec<String>,
    pub head: Option<git::HeadTarget>,
    pub head_commit: Option<git::Oid>,
}

impl Folder {
    pub fn note_problem(&self, path: &str, why: String) {
        lock(&self.problems).files.insert(path.to_owned(), why);
    }

    pub fn problems(&self) -> Problems {
        lock(&self.problems).clone()
    }

    pub fn summary(&self) -> Summary {
        let state = lock(&self.state);
        let (files, bytes) = state.totals();
        let rules = lock(&self.rules).clone();
        let pending = state
            .pending()
            .iter()
            .filter(|path| !matches!(state.work(path, &rules), Work::Done))
            .count()
            + state.awaiting().count();
        Summary {
            files,
            bytes,
            pending,
            conflicts: state.conflicts(),
            head: self
                .git
                .borrow()
                .as_ref()
                .and_then(|git| git.head.as_ref().map(|head| head.target.clone())),
            head_commit: self.head_commit(),
        }
    }
}
