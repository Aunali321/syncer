//! Following peers: their index after a cursor, and what each is waiting on.

use crate::{
    folder::{
        index::{Entry, path_problem},
        state::Cursor,
    },
    git::{self},
    ids::DeviceId,
    util::lock,
};
use std::collections::BTreeSet;
use tokio::sync::watch;

use super::*;

impl Folder {
    pub fn seq(&self) -> watch::Receiver<u64> {
        self.seq.subscribe()
    }

    /// The entries changed after `since`, and the cursor after them.
    pub fn since(&self, since: u64, limit: usize) -> (Vec<Entry>, u64) {
        lock(&self.state).index.since(since, limit)
    }

    pub fn cursor(&self, peer: &DeviceId) -> Cursor {
        lock(&self.state)
            .cursors
            .get(peer)
            .copied()
            .unwrap_or_default()
    }

    /// Whether a peer's changes, made while its checkout was at `head`, can
    /// land here: only when both checkouts are at the same commit, so two
    /// histories' work trees never mix.
    pub fn accepts(&self, head: Option<&git::Oid>) -> bool {
        self.head_commit().as_ref() == head
    }

    /// Merges entries a peer sent and moves its cursor. A peer never sends a
    /// path that breaks the naming rule; one that does (an older or broken
    /// peer) has those entries dropped.
    pub fn merge(&self, peer: &DeviceId, entries: &[Entry], cursor: Cursor) {
        let valid: Vec<Entry> = entries
            .iter()
            .filter(|entry| match path_problem(&entry.path) {
                None => true,
                Some(why) => {
                    tracing::warn!(
                        "{}: dropped {:?} from {}: {why}",
                        self.id,
                        entry.path,
                        crate::ids::short(peer)
                    );
                    false
                }
            })
            .cloned()
            .collect();
        {
            let mut state = lock(&self.state);
            state.merge(&valid);
            state.cursors.insert(*peer, cursor);
        }
        self.changed();
        self.work.notify_one();
    }

    /// Notes that this device has read `peer`'s whole index.
    pub fn caught_up_with(&self, peer: &DeviceId) {
        lock(&self.caught_up).insert(*peer);
    }

    /// The peers whose whole index this device has read since it started.
    pub fn caught_up(&self) -> BTreeSet<DeviceId> {
        lock(&self.caught_up).clone()
    }

    pub fn set_waiting(&self, peer: &DeviceId, why: Option<String>) {
        let mut problems = lock(&self.problems);
        match why {
            Some(why) => problems.waiting.insert(*peer, why),
            None => problems.waiting.remove(peer),
        };
    }
}
