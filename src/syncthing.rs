//! Noticing folders Syncthing already syncs. Two tools syncing the same files
//! fight over every change, so a folder that overlaps an active Syncthing
//! folder does not start. See DESIGN.md, "Syncthing".

use std::path::{Path, PathBuf};

/// Why syncer must leave `path` alone, if an active Syncthing folder
/// overlaps it.
pub fn managing(path: &Path) -> Option<String> {
    let config = config_file()?;
    let text = std::fs::read_to_string(&config).ok()?;
    let folders = parse(&text);
    let path = path.canonicalize().unwrap_or_else(|_| path.to_owned());

    folders
        .into_iter()
        .filter(|folder| !folder.paused)
        .find(|folder| {
            let theirs = folder.path.canonicalize().unwrap_or_else(|_| folder.path.clone());
            path.starts_with(&theirs) || theirs.starts_with(&path)
        })
        .map(|folder| {
            format!(
                "Syncthing syncs this directory as its folder {:?} at {}. Pause or remove it in \
                 Syncthing first; two tools syncing the same files fight over every change",
                folder.label,
                folder.path.display()
            )
        })
}

#[derive(Debug, PartialEq, Eq)]
struct SyncthingFolder {
    label: String,
    path: PathBuf,
    paused: bool,
}

fn parse(text: &str) -> Vec<SyncthingFolder> {
    let Ok(document) = roxmltree::Document::parse(text) else {
        return Vec::new();
    };

    document
        .root_element()
        .children()
        .filter(|node| node.has_tag_name("folder"))
        .filter_map(|node| {
            let path = node.attribute("path")?;
            let label = node
                .attribute("label")
                .filter(|label| !label.is_empty())
                .or_else(|| node.attribute("id"))
                .unwrap_or_default();
            let paused = node.attribute("paused") == Some("true")
                || node
                    .children()
                    .any(|child| child.has_tag_name("paused") && child.text() == Some("true"));
            Some(SyncthingFolder {
                label: label.to_owned(),
                path: expand_home(path),
                paused,
            })
        })
        .collect()
}

fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~") {
        Some(rest) => match dirs::home_dir() {
            Some(home) => home.join(rest.trim_start_matches(['/', '\\'])),
            None => PathBuf::from(path),
        },
        None => PathBuf::from(path),
    }
}

/// Where Syncthing keeps its config on this platform, if it is there.
fn config_file() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("STHOMEDIR") {
        return Some(PathBuf::from(home).join("config.xml"));
    }

    let candidates = [
        dirs::state_dir().map(|dir| dir.join("syncthing")),
        dirs::config_dir().map(|dir| dir.join("syncthing")),
        dirs::config_dir().map(|dir| dir.join("Syncthing")),
        dirs::data_local_dir().map(|dir| dir.join("Syncthing")),
    ];

    candidates
        .into_iter()
        .flatten()
        .map(|dir| dir.join("config.xml"))
        .find(|file| file.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_folders_and_their_pause() {
        let text = r#"<configuration version="37">
            <folder id="abcd-1234" label="Code" path="/home/me/code" type="sendreceive">
                <paused>false</paused>
            </folder>
            <folder id="efgh" label="" path="/data/photos" paused="true"></folder>
            <device id="X"></device>
        </configuration>"#;

        assert_eq!(
            parse(text),
            vec![
                SyncthingFolder {
                    label: "Code".into(),
                    path: "/home/me/code".into(),
                    paused: false
                },
                SyncthingFolder {
                    label: "efgh".into(),
                    path: "/data/photos".into(),
                    paused: true
                },
            ]
        );
        assert!(parse("not xml").is_empty());
    }
}
