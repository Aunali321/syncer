//! The devices that trust each other and the folders they share: a small map
//! every member holds a copy of, merged last writer wins. See DESIGN.md,
//! "Cluster state".

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
}

/// The replicated map.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterState {
    pub devices: BTreeMap<DeviceId, Lww<DeviceInfo>>,
    pub folders: BTreeMap<FolderId, Lww<FolderInfo>>,
    /// Which devices hold which folders.
    pub members: BTreeMap<FolderId, BTreeMap<DeviceId, Lww<bool>>>,
}

/// This device's copy of the cluster.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cluster {
    me: DeviceId,
    lamport: u64,
    state: ClusterState,
}

fn merge_one<T: Clone>(mine: &mut Option<&mut Lww<T>>, theirs: &Lww<T>) -> Option<Lww<T>> {
    match mine {
        Some(mine) if mine.stamp >= theirs.stamp => None,
        _ => Some(theirs.clone()),
    }
}

impl Cluster {
    /// A cluster of one: this device, named `name`.
    pub fn new(me: DeviceId, name: &str) -> Self {
        let mut cluster = Self {
            me,
            lamport: 0,
            state: ClusterState::default(),
        };
        cluster.set_device(me, name, false);
        cluster
    }

    pub fn me(&self) -> DeviceId {
        self.me
    }

    pub fn state(&self) -> &ClusterState {
        &self.state
    }

    fn stamp(&mut self) -> Stamp {
        self.lamport += 1;
        Stamp {
            lamport: self.lamport,
            by: self.me,
        }
    }

    /// Merges another member's copy. Returns whether anything changed.
    pub fn merge(&mut self, theirs: &ClusterState) -> bool {
        let mut changed = false;
        let mut seen = 0;

        for (id, record) in &theirs.devices {
            seen = seen.max(record.stamp.lamport);
            if let Some(newer) = merge_one(&mut self.state.devices.get_mut(id), record) {
                self.state.devices.insert(*id, newer);
                changed = true;
            }
        }

        for (id, record) in &theirs.folders {
            seen = seen.max(record.stamp.lamport);
            if let Some(newer) = merge_one(&mut self.state.folders.get_mut(id), record) {
                self.state.folders.insert(id.clone(), newer);
                changed = true;
            }
        }

        for (folder, members) in &theirs.members {
            let mine = self.state.members.entry(folder.clone()).or_default();
            for (device, record) in members {
                seen = seen.max(record.stamp.lamport);
                // Only a device says what it holds.
                if record.stamp.by != *device {
                    continue;
                }
                if let Some(newer) = merge_one(&mut mine.get_mut(device), record) {
                    mine.insert(*device, newer);
                    changed = true;
                }
            }
        }

        self.lamport = self.lamport.max(seen);
        changed
    }

    pub fn set_device(&mut self, id: DeviceId, name: &str, removed: bool) {
        let stamp = self.stamp();
        self.state.devices.insert(
            id,
            Lww {
                value: DeviceInfo {
                    name: name.to_owned(),
                    removed,
                },
                stamp,
            },
        );
    }

    pub fn set_folder(&mut self, id: FolderId, label: &str, settings: Settings, removed: bool) {
        let stamp = self.stamp();
        self.state.folders.insert(
            id,
            Lww {
                value: FolderInfo {
                    label: label.to_owned(),
                    settings,
                    removed,
                },
                stamp,
            },
        );
    }

    /// Records whether this device holds `folder`.
    pub fn set_holding(&mut self, folder: &FolderId, holds: bool) {
        let stamp = self.stamp();
        self.state
            .members
            .entry(folder.clone())
            .or_default()
            .insert(
                self.me,
                Lww {
                    value: holds,
                    stamp,
                },
            );
    }

    pub fn device(&self, id: &DeviceId) -> Option<&DeviceInfo> {
        self.state.devices.get(id).map(|record| &record.value)
    }

    /// Whether `id` is in the cluster and was not removed.
    pub fn is_member(&self, id: &DeviceId) -> bool {
        self.device(id).is_some_and(|device| !device.removed)
    }

    /// The members other than this device.
    pub fn others(&self) -> Vec<DeviceId> {
        self.state
            .devices
            .iter()
            .filter(|(id, record)| **id != self.me && !record.value.removed)
            .map(|(id, _)| *id)
            .collect()
    }

    pub fn name_of(&self, id: &DeviceId) -> String {
        self.device(id)
            .map(|device| device.name.clone())
            .unwrap_or_else(|| crate::ids::short(id))
    }

    pub fn folder(&self, id: &FolderId) -> Option<&FolderInfo> {
        self.state
            .folders
            .get(id)
            .map(|record| &record.value)
            .filter(|folder| !folder.removed)
    }

    /// The folders that were not removed.
    pub fn folders(&self) -> impl Iterator<Item = (&FolderId, &FolderInfo)> {
        self.state
            .folders
            .iter()
            .map(|(id, record)| (id, &record.value))
            .filter(|(_, folder)| !folder.removed)
    }

    /// The members other than this device that hold `folder`.
    pub fn holders(&self, folder: &FolderId) -> Vec<DeviceId> {
        self.state
            .members
            .get(folder)
            .into_iter()
            .flatten()
            .filter(|(device, record)| {
                record.value && **device != self.me && self.is_member(device)
            })
            .map(|(device, _)| *device)
            .collect()
    }

    /// Whether `device` holds `folder`.
    pub fn holds(&self, device: &DeviceId, folder: &FolderId) -> bool {
        self.state
            .members
            .get(folder)
            .and_then(|members| members.get(device))
            .is_some_and(|record| record.value)
    }

    /// Finds a folder by id or label.
    pub fn find_folder(&self, query: &str) -> Option<FolderId> {
        if let Ok(id) = query.parse::<FolderId>()
            && self.folder(&id).is_some()
        {
            return Some(id);
        }

        let mut found = self.folders().filter(|(_, folder)| folder.label == query);
        let first = found.next()?;
        // An ambiguous label finds nothing.
        found.next().is_none().then(|| first.0.clone())
    }

    /// Finds a device by id, short id or name.
    pub fn find_device(&self, query: &str) -> Option<DeviceId> {
        let mut found = self.state.devices.iter().filter(|(id, record)| {
            id.to_string() == query || crate::ids::short(id) == query || record.value.name == query
        });
        let first = found.next()?;
        found.next().is_none().then_some(*first.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(byte: u8) -> DeviceId {
        iroh::SecretKey::from_bytes(&[byte; 32]).public()
    }

    #[test]
    fn copies_converge_and_the_later_write_wins() {
        let (a, b) = (device(1), device(2));
        let mut on_a = Cluster::new(a, "laptop");
        let mut on_b = Cluster::new(b, "desk");
        on_a.merge(on_b.state());
        on_b.merge(on_a.state());
        assert_eq!(on_a.state(), on_b.state());

        let folder = FolderId::random();
        on_a.set_folder(folder.clone(), "notes", Settings::default(), false);
        on_a.set_holding(&folder, true);
        on_b.merge(on_a.state());
        on_b.set_folder(folder.clone(), "renamed", Settings::default(), false);
        on_a.merge(on_b.state());

        assert_eq!(on_a.state(), on_b.state());
        assert_eq!(
            on_a.folder(&folder).map(|f| f.label.as_str()),
            Some("renamed")
        );
        assert_eq!(on_b.holders(&folder), vec![a]);
        assert!(on_a.holders(&folder).is_empty());
    }

    #[test]
    fn removed_devices_stop_counting() {
        let (a, b) = (device(1), device(2));
        let mut cluster = Cluster::new(a, "laptop");
        cluster.set_device(b, "desk", false);
        let folder = FolderId::random();
        cluster
            .state
            .members
            .entry(folder.clone())
            .or_default()
            .insert(
                b,
                Lww {
                    value: true,
                    stamp: Stamp { lamport: 1, by: b },
                },
            );
        assert_eq!(cluster.holders(&folder), vec![b]);

        cluster.set_device(b, "desk", true);
        assert!(!cluster.is_member(&b));
        assert!(cluster.holders(&folder).is_empty());
        assert!(cluster.others().is_empty());
    }

    #[test]
    fn folders_and_devices_are_found_by_name() {
        let (a, b) = (device(1), device(2));
        let mut cluster = Cluster::new(a, "laptop");
        cluster.set_device(b, "desk", false);
        let folder = FolderId::random();
        cluster.set_folder(folder.clone(), "code", Settings::default(), false);

        assert_eq!(cluster.find_folder("code"), Some(folder.clone()));
        assert_eq!(cluster.find_folder(folder.as_str()), Some(folder));
        assert_eq!(cluster.find_device("desk"), Some(b));
        assert_eq!(cluster.find_device(&crate::ids::short(&b)), Some(b));
        assert_eq!(cluster.find_device("nobody"), None);
    }

    #[test]
    fn only_a_device_says_what_it_holds() {
        let (a, b, c) = (device(1), device(2), device(3));
        let mut on_a = Cluster::new(a, "laptop");
        let folder = FolderId::random();
        let mut forged = ClusterState::default();
        forged.members.entry(folder.clone()).or_default().insert(
            c,
            Lww {
                value: true,
                stamp: Stamp { lamport: 9, by: b },
            },
        );
        on_a.merge(&forged);
        assert!(!on_a.holds(&c, &folder));
    }

    #[test]
    fn the_state_survives_json() {
        let mut cluster = Cluster::new(device(1), "laptop");
        cluster.set_folder(FolderId::random(), "code", Settings::default(), false);
        let json = serde_json::to_string(&cluster).expect("encodes");
        let back: Cluster = serde_json::from_str(&json).expect("decodes");
        assert_eq!(back.state(), cluster.state());
    }
}
