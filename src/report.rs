//! What `syncer status` shows: this device, its peers, and every folder it
//! holds or is offered.

use crate::{
    folder::live::{Problems, Summary},
    git::HeadTarget,
    ids::short,
    service::Service,
    util::bytes,
};
use serde::{Deserialize, Serialize};
use std::fmt::{self, Write as _};

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
    pub connected: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderReport {
    pub id: String,
    pub label: String,
    pub path: String,
    pub state: String,
    pub summary: Option<Summary>,
    pub problems: Option<Problems>,
    /// The other devices holding it, by name, with why their changes wait.
    pub peers: Vec<(String, Option<String>)>,
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
        connected: node.is_connected(id),
    };

    let folders = config
        .folders
        .iter()
        .map(|(id, local)| {
            let label = cluster.folder(id).map_or_else(|| id.to_string(), |folder| folder.label.clone());
            let running = service.folder(id);
            let problems = running.as_ref().map(|folder| folder.problems());
            let summary = running.as_ref().map(|folder| folder.summary());
            let state = if local.paused {
                "paused".to_owned()
            } else if let Some(why) = failed.get(id) {
                format!("stopped: {why}")
            } else if cluster.folder(id).is_none() {
                "removed from the cluster".to_owned()
            } else if let Some(summary) = &summary {
                if summary.pending > 0 { format!("syncing, {} to do", summary.pending) } else { "up to date".to_owned() }
            } else {
                "starting".to_owned()
            };
            let peers = cluster
                .holders(id)
                .iter()
                .map(|peer| {
                    let waiting = problems.as_ref().and_then(|problems| problems.waiting.get(peer).cloned());
                    (cluster.name_of(peer), waiting)
                })
                .collect();

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
            from: cluster.holders(id).iter().map(|peer| cluster.name_of(peer)).collect(),
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

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "This device: {} ({})", self.me.name, self.me.short)?;

        if self.devices.is_empty() {
            writeln!(f, "No other devices yet. Run `syncer invite` here and `syncer join` on another.")?;
        } else {
            writeln!(f, "\nDevices")?;
            for device in &self.devices {
                let state = if device.connected { "connected" } else { "not connected" };
                writeln!(f, "  {} ({}): {state}", device.name, device.short)?;
            }
        }

        for folder in &self.folders {
            writeln!(f, "\n{} [{}]  {}", folder.label, folder.id, folder.path)?;
            writeln!(f, "  {}", folder.state)?;

            if let Some(summary) = &folder.summary {
                let mut line = format!("  {} files, {}", summary.files, bytes(summary.bytes));
                match &summary.head {
                    Some(HeadTarget::Branch(branch)) => write!(line, "; on branch {branch}").ok(),
                    Some(HeadTarget::Detached(oid)) => write!(line, "; detached at {}", &oid.as_str()[..12]).ok(),
                    None => None,
                };
                writeln!(f, "{line}")?;
                if !summary.conflicts.is_empty() {
                    writeln!(f, "  Conflict copies (look at them, then delete them):")?;
                    for path in &summary.conflicts {
                        writeln!(f, "    {path}")?;
                    }
                }
            }

            for (peer, waiting) in &folder.peers {
                match waiting {
                    Some(why) => writeln!(f, "  with {peer}: waiting, {why}")?,
                    None => writeln!(f, "  with {peer}")?,
                }
            }

            if let Some(problems) = &folder.problems {
                for note in &problems.git {
                    writeln!(f, "  Git: {note}")?;
                }
                for (path, why) in problems.files.iter().take(10) {
                    writeln!(f, "  {path}: {why}")?;
                }
                if problems.files.len() > 10 {
                    writeln!(f, "  and {} more paths that could not be synced", problems.files.len() - 10)?;
                }
            }
        }

        if !self.offers.is_empty() {
            writeln!(f, "\nOffered to this device (take one with `syncer accept <id> <path>`):")?;
            for offer in &self.offers {
                writeln!(f, "  {} [{}] from {}", offer.label, offer.id, offer.from.join(", "))?;
            }
        }

        Ok(())
    }
}
