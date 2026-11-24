//! How a file system compares names, found by trying names out.

use std::{fs, path::Path};

/// Which names a file system treats as one. Names stay as written; two that
/// fold to the same key cannot both be stored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Folding {
    /// `README` and `readme` are one name: macOS and Windows by default.
    pub case: bool,
    /// `name.` and `name ` are stored as `name`: FAT, and Windows' own calls.
    pub trailing: bool,
    /// A name's Unicode forms (`é` as one code point or as `e` and an
    /// accent) are one name: macOS.
    pub unicode: bool,
}

impl Folding {
    /// Tries names out in `dir`, an empty scratch directory on the file
    /// system in question.
    pub fn probe(dir: &Path) -> Self {
        let same = |written: &str, other: &str| {
            let probe = dir.join(written);
            if fs::write(&probe, b"").is_err() {
                return false;
            }
            let one = fs::symlink_metadata(dir.join(other)).is_ok();
            let _ = fs::remove_file(&probe);
            one
        };
        Self {
            case: same("case-probe", "CASE-PROBE"),
            trailing: same("trail-probe.", "trail-probe"),
            unicode: same("\u{e9}-probe", "e\u{301}-probe"),
        }
    }

    /// The key under which this file system finds `path`.
    pub fn key(&self, path: &str) -> String {
        let fold = |part: &str| {
            let part = if self.trailing {
                part.trim_end_matches(['.', ' '])
            } else {
                part
            };
            let part: String = if self.unicode {
                unicode_normalization::UnicodeNormalization::nfc(part).collect()
            } else {
                part.to_owned()
            };
            if self.case { part.to_lowercase() } else { part }
        };
        path.split('/').map(fold).collect::<Vec<_>>().join("/")
    }

    /// Why this file system, or the system around it, cannot store `path`
    /// under that name, if it cannot.
    pub fn refuses(&self, path: &str) -> Option<&'static str> {
        path.split('/').find_map(|part| {
            if self.trailing && part.ends_with(['.', ' ']) {
                Some("ends in a dot or space (not allowed on this drive)")
            } else if cfg!(windows) && crate::folder::index::windows_refuses(part) {
                Some("name not allowed on Windows")
            } else {
                None
            }
        })
    }
}
