//! Git sync: branches, tags and HEAD move as Git data, never as files.

pub mod config;
pub mod repo;
pub mod sync;

pub use repo::{HeadTarget, Oid, Repo};
pub use sync::{Diverged, GitState, Memory};
