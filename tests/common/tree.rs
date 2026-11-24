//! Comparing devices' folders file by file.

use std::{collections::BTreeMap, fs, path::Path};

/// One entry of a folder as the file system shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    File { content: Vec<u8>, executable: bool },
    Link(String),
}

/// Every file and symlink under `root`, by `/`-separated path, leaving out
/// syncer's own directory and `.git` directories.
pub fn tree(root: &Path) -> BTreeMap<String, Node> {
    let mut out = BTreeMap::new();
    walk(root, "", &mut out);
    out
}

fn walk(root: &Path, dir: &str, out: &mut BTreeMap<String, Node>) {
    let full = if dir.is_empty() {
        root.to_owned()
    } else {
        root.join(dir)
    };
    let Ok(entries) = fs::read_dir(&full) else {
        return;
    };

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = if dir.is_empty() {
            name.clone()
        } else {
            format!("{dir}/{name}")
        };
        if (dir.is_empty() && name == ".syncer") || name == ".git" {
            continue;
        }
        let Ok(meta) = entry.path().symlink_metadata() else {
            continue;
        };
        if meta.file_type().is_symlink() {
            let target = fs::read_link(entry.path()).expect("a link");
            out.insert(path, Node::Link(target.to_string_lossy().into_owned()));
        } else if meta.is_dir() {
            walk(root, &path, out);
        } else {
            #[cfg(unix)]
            let executable = {
                use std::os::unix::fs::PermissionsExt;
                meta.permissions().mode() & 0o111 != 0
            };
            #[cfg(not(unix))]
            let executable = false;
            out.insert(
                path,
                Node::File {
                    content: fs::read(entry.path()).unwrap_or_default(),
                    executable,
                },
            );
        }
    }
}

/// The paths where two trees differ, for a failure message.
pub fn difference(a: &BTreeMap<String, Node>, b: &BTreeMap<String, Node>) -> Vec<String> {
    let mut paths: Vec<String> = a
        .keys()
        .chain(b.keys())
        .filter(|path| a.get(*path) != b.get(*path))
        .cloned()
        .collect();
    paths.sort();
    paths.dedup();
    paths
        .into_iter()
        .map(|path| {
            let show = |node: Option<&Node>| match node {
                None => "missing".to_owned(),
                Some(Node::Link(target)) => format!("link to {target}"),
                Some(Node::File {
                    content,
                    executable,
                }) => format!(
                    "{:?}{}",
                    String::from_utf8_lossy(&content[..content.len().min(40)]),
                    if *executable { " (exec)" } else { "" }
                ),
            };
            format!("{path}: {} vs {}", show(a.get(&path)), show(b.get(&path)))
        })
        .collect()
}

/// The files directly in or under `root` (not directories), by path.
pub fn files(root: &Path) -> Vec<String> {
    tree(root).into_keys().collect()
}
