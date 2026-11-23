//! One shared folder on this device.

pub mod disk;
pub mod index;
pub mod rules;
pub mod scan;

/// Syncer's own directory in a folder root: the folder marker and temporary
/// files. Never synced.
pub const OWN_DIR: &str = ".syncer";
