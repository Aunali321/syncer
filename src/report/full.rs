//! The full status: every folder and device, with counts and paths.

use super::{FolderState, Report, says::files};
use crate::{git::HeadTarget, util::bytes};
use std::fmt::{self, Write as _};

/// The whole status: every folder and device.
pub struct Full<'a>(pub &'a Report);

impl fmt::Display for Full<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let report = self.0;
        writeln!(f, "This device: {} ({})", report.me.name, report.me.short)?;

        if !report.devices.is_empty() {
            writeln!(f, "\nDevices")?;
            for device in &report.devices {
                let state = match (device.link, device.line()) {
                    (_, None) => "connected".to_owned(),
                    (Some(_), Some(line)) => line,
                    (None, Some(line)) => format!("not connected, {line}"),
                };
                writeln!(f, "  {} ({}): {state}", device.name, device.short)?;
            }
        }

        for folder in &report.folders {
            writeln!(f, "\n{} [{}]  {}", folder.label, folder.id, folder.path)?;
            if matches!(folder.state, FolderState::UpToDate | FolderState::HeldBack) {
                writeln!(f, "  up to date")?;
            }
            for line in folder.lines(usize::MAX) {
                writeln!(f, "  {line}")?;
            }
            if let Some(summary) = &folder.summary {
                let count = usize::try_from(summary.files).unwrap_or(usize::MAX);
                let mut line = format!("  {count} {}, {}", files(count), bytes(summary.bytes));
                match &summary.head {
                    Some(HeadTarget::Branch(branch)) => write!(line, ", on branch {branch}").ok(),
                    Some(HeadTarget::Detached(oid)) => {
                        write!(line, ", detached at {}", &oid.as_str()[..12]).ok()
                    }
                    None => None,
                };
                writeln!(f, "{line}")?;
            }
            for peer in &folder.peers {
                writeln!(f, "  with {}", peer.name)?;
            }
        }

        if !report.offers.is_empty() {
            writeln!(f, "\nOffered to this device")?;
            for offer in &report.offers {
                writeln!(
                    f,
                    "  {} [{}] from {}. Run: syncer accept {}",
                    offer.label,
                    offer.id,
                    offer.from.join(", "),
                    offer.label
                )?;
            }
        }

        Ok(())
    }
}
