//! Watching a folder for changes: which paths, or the repository's refs.

use crate::folder::OWN_DIR;
use notify::{EventKind, RecursiveMode, Watcher as _};
use std::path::{Component, Path};
use tokio::sync::mpsc;

/// What the watcher reports.
pub enum Event {
    Paths(Vec<String>),
    Git,
    Everything,
}

/// Watches `root`, sending what changed. `None` when the folder cannot be
/// watched (too many directories for the system's limit, say); it is then
/// scanned on a timer.
pub fn watch_folder(
    root: &Path,
    events: mpsc::UnboundedSender<Event>,
) -> Option<notify::RecommendedWatcher> {
    let root_owned = root.canonicalize().unwrap_or_else(|_| root.to_owned());
    let handler = move |result: notify::Result<notify::Event>| {
        let event = match result {
            Ok(event) if event.need_rescan() => Event::Everything,
            Ok(event) if matches!(event.kind, EventKind::Access(_)) => return,
            Ok(event) => {
                let mut paths = Vec::new();
                let mut git = false;
                for path in &event.paths {
                    match classify(&root_owned, path) {
                        Some(Seen::File(path)) => paths.push(path),
                        Some(Seen::Git) => git = true,
                        None => {}
                    }
                }
                if !paths.is_empty() {
                    let _ = events.send(Event::Paths(paths));
                }
                if git {
                    let _ = events.send(Event::Git);
                }
                return;
            }
            Err(_) => Event::Everything,
        };
        let _ = events.send(event);
    };

    let mut watcher = match notify::recommended_watcher(handler) {
        Ok(watcher) => watcher,
        Err(e) => {
            tracing::warn!(
                "{}: not watched, scanning every minute instead: {e}",
                root.display()
            );
            return None;
        }
    };

    match watcher.watch(root, RecursiveMode::Recursive) {
        Ok(()) => Some(watcher),
        Err(e) => {
            tracing::warn!(
                "{}: not watched, scanning every minute instead: {e}",
                root.display()
            );
            None
        }
    }
}

enum Seen {
    File(String),
    Git,
}

/// What an event at `path` concerns: a folder path, the repository's refs
/// or HEAD, or nothing syncer follows.
fn classify(root: &Path, path: &Path) -> Option<Seen> {
    let relative = path.strip_prefix(root).ok()?;
    let parts: Vec<&str> = relative
        .components()
        .map(|part| match part {
            Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect::<Option<_>>()?;

    match parts.first().copied()? {
        OWN_DIR => None,
        ".git" => match parts.get(1).copied() {
            Some(
                "HEAD" | "refs" | "packed-refs" | "MERGE_HEAD" | "rebase-merge" | "rebase-apply"
                | "index.lock",
            ) => Some(Seen::Git),
            _ => None,
        },
        _ => Some(Seen::File(parts.join("/"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_sorted_by_what_they_touch() {
        let root = Path::new("/w");
        assert!(
            matches!(classify(root, Path::new("/w/src/a.rs")), Some(Seen::File(path)) if path == "src/a.rs")
        );
        assert!(matches!(
            classify(root, Path::new("/w/.git/refs/heads/main")),
            Some(Seen::Git)
        ));
        assert!(classify(root, Path::new("/w/.git/objects/ab/cd")).is_none());
        assert!(classify(root, Path::new("/w/.syncer/tmp/x")).is_none());
        assert!(classify(root, Path::new("/elsewhere")).is_none());
    }
}
