//! What `syncer status` shows. By default only what needs attention, with
//! what to do about it; `Full` lists every folder and device. This part
//! gathers it; the others write it out.

mod full;
mod says;
mod short;

pub use full::Full;

use crate::{
    folder::live::{GitNotes, Problems, Stopped, Summary},
    ids::short,
    net::node::Link,
    service::Service,
};
use serde::{Deserialize, Serialize};
use std::time::SystemTime;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub me: Device,
    pub devices: Vec<Device>,
    pub folders: Vec<FolderReport>,
    pub offers: Vec<Offer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub short: String,
    pub name: String,
    /// How the open connection reaches it; `None` when not connected.
    pub link: Option<Link>,
    /// When it was last heard from, if ever.
    pub last_seen: Option<SystemTime>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderReport {
    pub id: String,
    pub label: String,
    pub path: String,
    pub state: FolderState,
    pub summary: Option<Summary>,
    pub problems: Option<Problems>,
    /// The other devices holding it.
    pub peers: Vec<PeerReport>,
}

/// Where a folder stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FolderState {
    UpToDate,
    Paused,
    Stopped(Stopped),
    /// Another device stopped sharing it.
    Removed,
    Starting,
    /// Nothing heard yet from any device holding it.
    Waiting,
    /// Files still to bring in line.
    Syncing {
        left: usize,
    },
    /// In line with what can be had: changes from some devices wait.
    HeldBack,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerReport {
    pub name: String,
    pub connected: bool,
    pub last_seen: Option<SystemTime>,
    /// Whether this device has read its whole index since it started.
    pub caught_up: bool,
    /// Why its changes are held back, if they are.
    pub waiting: Option<String>,
    pub git: GitNotes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Offer {
    pub id: String,
    pub label: String,
    pub from: Vec<String>,
}

pub fn build(service: &Service) -> Report {
    let cluster = service.cluster();
    let config = service.config();
    let failed = service.failed();
    let node = service.node();
    let device = |id: &crate::ids::DeviceId| Device {
        id: id.to_string(),
        short: short(id),
        name: cluster.name_of(id),
        link: node.link(id),
        last_seen: node.last_seen(id),
    };

    let folders = config
        .folders
        .iter()
        .map(|(id, local)| {
            let label = cluster
                .folder(id)
                .map_or_else(|| id.to_string(), |folder| folder.label.clone());
            let running = service.folder(id);
            let problems = running.as_ref().map(|folder| folder.problems());
            let summary = running.as_ref().map(|folder| folder.summary());
            let caught_up = running
                .as_ref()
                .map(|folder| folder.caught_up())
                .unwrap_or_default();
            let peers: Vec<PeerReport> = cluster
                .holders(id)
                .iter()
                .map(|peer| PeerReport {
                    name: cluster.name_of(peer),
                    connected: node.is_connected(peer),
                    last_seen: node.last_seen(peer),
                    caught_up: caught_up.contains(peer),
                    waiting: problems
                        .as_ref()
                        .and_then(|problems| problems.waiting.get(peer).cloned()),
                    git: problems
                        .as_ref()
                        .and_then(|problems| problems.git.get(peer).cloned())
                        .unwrap_or_default(),
                })
                .collect();
            let stopped = failed.get(id).cloned().or_else(|| {
                problems
                    .as_ref()
                    .and_then(|problems| problems.folder.clone())
            });

            let state = if local.paused {
                FolderState::Paused
            } else if let Some(stopped) = stopped {
                FolderState::Stopped(stopped)
            } else if cluster.folder(id).is_none_or(|folder| folder.removed) {
                FolderState::Removed
            } else if let Some(summary) = &summary {
                let heard = peers.is_empty() || peers.iter().any(|peer| peer.caught_up);
                let held = peers.iter().any(|peer| peer.waiting.is_some());
                match summary.pending {
                    0 if !heard => FolderState::Waiting,
                    0 if held => FolderState::HeldBack,
                    0 => FolderState::UpToDate,
                    left => FolderState::Syncing { left },
                }
            } else {
                FolderState::Starting
            };

            FolderReport {
                id: id.to_string(),
                label,
                path: local.path.display().to_string(),
                state,
                summary,
                problems,
                peers,
            }
        })
        .collect();

    let offers = cluster
        .folders()
        .filter(|(id, _)| !config.folders.contains_key(*id))
        .map(|(id, folder)| Offer {
            id: id.to_string(),
            label: folder.label.clone(),
            from: cluster
                .holders(id)
                .iter()
                .map(|peer| cluster.name_of(peer))
                .collect(),
        })
        .filter(|offer| !offer.from.is_empty())
        .collect();

    Report {
        me: device(&cluster.me()),
        devices: cluster.others().iter().map(device).collect(),
        folders,
        offers,
    }
}
