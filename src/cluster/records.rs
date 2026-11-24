//! What the cluster replicates: each value with the stamp that orders it.

use crate::{
    folder::Settings,
    ids::{DeviceId, FolderId},
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// When a value was written and by whom: the higher stamp wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Stamp {
    pub lamport: u64,
    pub by: DeviceId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lww<T> {
    pub value: T,
    pub stamp: Stamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub name: String,
    pub removed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderInfo {
    pub label: String,
    pub settings: Settings,
    pub removed: bool,
    /// Where the device that shared it keeps it, from its home directory,
    /// joined with `/`: other devices put it in the same place by default.
    #[serde(default)]
    pub home_path: Option<String>,
}

/// The replicated map.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterState {
    pub devices: BTreeMap<DeviceId, Lww<DeviceInfo>>,
    pub folders: BTreeMap<FolderId, Lww<FolderInfo>>,
    /// Which devices hold which folders.
    pub members: BTreeMap<FolderId, BTreeMap<DeviceId, Lww<bool>>>,
}
