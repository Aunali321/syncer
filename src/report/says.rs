//! What to say about a folder or a device, for both forms of the status.

use super::{Device, FolderReport, FolderState};
use crate::{folder::live::Stopped, net::node::Link};
use std::time::SystemTime;

/// How many paths of one kind the short status lists.
pub const LISTED: usize = 5;

impl FolderReport {
    /// What to say about the folder: its state, then what needs attention,
    /// at most `listed` paths of each kind. Nothing when it is up to date
    /// and well.
    pub fn lines(&self, listed: usize) -> Vec<String> {
        let mut lines = Vec::new();
        let offline = !self.peers.is_empty() && self.peers.iter().all(|peer| !peer.connected);
        let names = || {
            self.peers
                .iter()
                .map(|peer| peer.name.clone())
                .collect::<Vec<_>>()
                .join(", ")
        };

        match &self.state {
            FolderState::UpToDate | FolderState::HeldBack => {}
            FolderState::Paused => lines.push("paused".to_owned()),
            FolderState::Stopped(Stopped::Missing) => lines.push(format!(
                "folder not found. If the drive is connected, run: syncer reset {}",
                self.label
            )),
            FolderState::Stopped(Stopped::Other(why)) => lines.push(format!("stopped: {why}")),
            FolderState::Removed => lines.push("no longer shared".to_owned()),
            FolderState::Starting => lines.push("starting".to_owned()),
            FolderState::Waiting if offline => {
                lines.push(format!("waiting for {} to connect", names()));
            }
            FolderState::Waiting => lines.push(format!("waiting for {}", names())),
            FolderState::Syncing { left } if offline => lines.push(format!(
                "{left} {} waiting for {} (offline)",
                files(*left),
                names()
            )),
            FolderState::Syncing { left } => {
                lines.push(format!("syncing, {left} {} left", files(*left)));
            }
        }

        for peer in &self.peers {
            for split in &peer.git.diverged {
                lines.push(format!(
                    "branch {} diverged from {}",
                    split.branch, peer.name
                ));
                lines.push(format!("  run: git merge {}", split.merge));
            }
            for note in &peer.git.notes {
                lines.push(format!("{}: {note}", peer.name));
            }
            // A device that is not connected says so under Devices.
            if peer.git.diverged.is_empty()
                && peer.connected
                && let Some(why) = &peer.waiting
            {
                lines.push(format!("waiting for {}: {why}", peer.name));
            }
        }

        if let Some(summary) = &self.summary
            && !summary.conflicts.is_empty()
        {
            let count = summary.conflicts.len();
            let noun = if count == 1 { "copy" } else { "copies" };
            lines.push(format!("{count} conflict {noun}"));
            list(
                &mut lines,
                summary.conflicts.iter().map(|path| shown(path)),
                listed,
            );
        }

        if let Some(problems) = &self.problems {
            for (paths, what) in [
                (&problems.files, "not synced"),
                (&problems.names, "can't sync (invalid name)"),
            ] {
                if paths.is_empty() {
                    continue;
                }
                lines.push(format!("{} {} {what}", paths.len(), files(paths.len())));
                let each = paths
                    .iter()
                    .map(|(path, why)| format!("{}: {why}", shown(path)));
                list(&mut lines, each, listed);
            }
        }

        lines
    }
}

impl Device {
    /// What to say about the device; nothing when it is connected directly.
    pub fn line(&self) -> Option<String> {
        match (self.link, self.last_seen) {
            (Some(Link::Direct), _) => None,
            (Some(Link::Relayed), _) => Some("connected via relay (slower)".to_owned()),
            (None, Some(at)) => Some(format!("last seen {}", ago(at))),
            (None, None) => Some("never connected".to_owned()),
        }
    }
}

/// Lines under the one before: up to `listed` of `items`, then how many
/// more there are.
fn list(lines: &mut Vec<String>, items: impl ExactSizeIterator<Item = String>, listed: usize) {
    let total = items.len();
    lines.extend(items.take(listed).map(|item| format!("  {item}")));
    if total > listed {
        lines.push(format!(
            "  and {} more (syncer status --all)",
            total - listed
        ));
    }
}

pub fn files(count: usize) -> &'static str {
    if count == 1 { "file" } else { "files" }
}

/// How long ago `at` was, for people: `3 days ago`.
fn ago(at: SystemTime) -> String {
    let secs = SystemTime::now()
        .duration_since(at)
        .map_or(0, |elapsed| elapsed.as_secs());
    let (count, unit) = match secs {
        0..60 => return "just now".to_owned(),
        60..3600 => (secs / 60, "minute"),
        3600..86_400 => (secs / 3600, "hour"),
        _ => (secs / 86_400, "day"),
    };
    let plural = if count == 1 { "" } else { "s" };
    format!("{count} {unit}{plural} ago")
}

/// A path as text a terminal shows as it is: control characters escaped.
fn shown(path: &str) -> String {
    path.chars()
        .map(|c| {
            if c.is_control() {
                c.escape_default().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}
