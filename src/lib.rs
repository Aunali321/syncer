//! Peer-to-peer folder sync between your own machines that understands Git.
//! DESIGN.md explains how it works.

pub mod folder;
pub mod git;

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Locks `mutex`, carrying on with what a holder that panicked left.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
