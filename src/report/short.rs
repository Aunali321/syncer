//! The short status: only what needs attention, with what to do about it.

use super::{Device, FolderReport, Report, says::LISTED};
use std::fmt;

/// Writes `label` in a column `width` wide, then `lines`, the later ones
/// under the first.
fn row(f: &mut fmt::Formatter<'_>, label: &str, width: usize, lines: &[String]) -> fmt::Result {
    for (at, line) in lines.iter().enumerate() {
        let name = if at == 0 { label } else { "" };
        writeln!(f, "  {name:width$}  {line}")?;
    }
    Ok(())
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let attention: Vec<(&FolderReport, Vec<String>)> = self
            .folders
            .iter()
            .map(|folder| (folder, folder.lines(LISTED)))
            .filter(|(_, lines)| !lines.is_empty())
            .collect();
        let devices: Vec<(&Device, String)> = self
            .devices
            .iter()
            .filter_map(|device| Some((device, device.line()?)))
            .collect();
        let width = attention
            .iter()
            .map(|(folder, _)| folder.label.len())
            .chain(self.offers.iter().map(|offer| offer.label.len()))
            .chain(devices.iter().map(|(device, _)| device.name.len()))
            .max()
            .unwrap_or(0);
        let well = self.folders.len() - attention.len();

        if self.folders.is_empty() && self.offers.is_empty() {
            writeln!(f, "No folders yet. Share one with: syncer add <path>")?;
        } else if attention.is_empty() && self.offers.is_empty() {
            match well {
                1 => writeln!(f, "{} is up to date.", self.folders[0].label)?,
                _ => writeln!(f, "All {well} folders are up to date.")?,
            }
        } else {
            writeln!(f, "Folders")?;
            for (folder, lines) in &attention {
                row(f, &folder.label, width, lines)?;
            }
            for offer in &self.offers {
                let lines = [
                    format!("offered by {}", offer.from.join(", ")),
                    format!("  run: syncer accept {}", offer.label),
                ];
                row(f, &offer.label, width, &lines)?;
            }
            match well {
                0 => {}
                1 => writeln!(f, "\n  1 other folder is up to date.")?,
                _ => writeln!(f, "\n  {well} other folders are up to date.")?,
            }
        }

        if self.devices.is_empty() {
            writeln!(
                f,
                "\nNo other devices yet. Run `syncer invite` here, then `syncer join` on your \
                 other device."
            )?;
        } else if !devices.is_empty() {
            writeln!(f, "\nDevices")?;
            for (device, line) in &devices {
                row(f, &device.name, width, std::slice::from_ref(line))?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        folder::live::{GitNotes, Split, Stopped},
        net::node::Link,
        report::{FolderState, Offer, PeerReport},
    };
    use std::time::{Duration, SystemTime};

    fn peer(name: &str, connected: bool) -> PeerReport {
        PeerReport {
            name: name.to_owned(),
            connected,
            last_seen: None,
            caught_up: connected,
            waiting: None,
            git: GitNotes::default(),
        }
    }

    fn folder(label: &str, state: FolderState, peers: Vec<PeerReport>) -> FolderReport {
        FolderReport {
            id: format!("{label}-id"),
            label: label.to_owned(),
            path: format!("/home/me/{label}"),
            state,
            summary: None,
            problems: None,
            peers,
        }
    }

    fn device(name: &str, link: Option<Link>, days_ago: Option<u64>) -> Device {
        Device {
            id: format!("{name}-id"),
            short: name.to_owned(),
            name: name.to_owned(),
            link,
            last_seen: days_ago
                .map(|days| SystemTime::now() - Duration::from_secs(days * 86_400 + 60)),
        }
    }

    fn report(folders: Vec<FolderReport>, devices: Vec<Device>, offers: Vec<Offer>) -> Report {
        Report {
            me: device("pc", None, None),
            devices,
            folders,
            offers,
        }
    }

    #[test]
    fn all_well_is_one_line() {
        let laptop = || vec![peer("laptop", true)];
        let shown = report(
            vec![
                folder("reseam", FolderState::UpToDate, laptop()),
                folder("patches", FolderState::UpToDate, laptop()),
            ],
            vec![device("laptop", Some(Link::Direct), Some(0))],
            vec![],
        )
        .to_string();

        assert_eq!(shown, "All 2 folders are up to date.\n");
    }

    #[test]
    fn only_what_needs_attention_is_listed() {
        let mut split = peer("laptop", true);
        split.git.diverged.push(Split {
            branch: "main".into(),
            merge: "syncer/337496cfc9/main".into(),
        });
        let shown = report(
            vec![
                folder("reseam", FolderState::HeldBack, vec![split]),
                folder(
                    "patches",
                    FolderState::Syncing { left: 120 },
                    vec![peer("x2", false)],
                ),
                folder("media", FolderState::Stopped(Stopped::Missing), vec![]),
                folder("notes", FolderState::UpToDate, vec![]),
            ],
            vec![
                device("laptop", Some(Link::Direct), Some(0)),
                device("x2", None, Some(3)),
            ],
            vec![Offer {
                id: "holder-id".into(),
                label: "holder".into(),
                from: vec!["laptop".into()],
            }],
        )
        .to_string();

        assert_eq!(
            shown,
            "\
Folders
  reseam   branch main diverged from laptop
             run: git merge syncer/337496cfc9/main
  patches  120 files waiting for x2 (offline)
  media    folder not found. If the drive is connected, run: syncer reset media
  holder   offered by laptop
             run: syncer accept holder

  1 other folder is up to date.

Devices
  x2       last seen 3 days ago
"
        );
    }
}
