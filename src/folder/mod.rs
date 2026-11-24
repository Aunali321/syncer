//! One shared folder on this device.

pub mod disk;
pub mod index;
pub mod live;
pub mod rules;
pub mod scan;
pub mod state;

pub use live::{Folder, Settings};

/// Syncer's own directory in a folder root: the folder marker and temporary
/// files. Never synced.
pub const OWN_DIR: &str = ".syncer";
/// The marker file in `OWN_DIR`, holding the folder id.
pub const MARKER: &str = "folder";
