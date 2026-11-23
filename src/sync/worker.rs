//! The task that keeps one folder in step: it watches the disk, scans what
//! changed, applies what peers changed, fetches content, follows each peer
//! that holds the folder and saves the state.

use crate::{
    folder::{Folder, OWN_DIR, index::Hash, scan::Scope},
    ids::DeviceId,
    net::node::Node,
    sync::{fetch, pull},
};
use notify::{EventKind, RecursiveMode, Watcher as _};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::{Component, Path},
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinSet,
    time::{Instant, interval, sleep_until},
};
use tokio_util::task::AbortOnDropHandle;

/// How long after the last file event a scan waits, and at most after the
/// first.
const SETTLE: Duration = Duration::from_millis(400);
const MAX_SETTLE: Duration = Duration::from_secs(3);
/// How often the whole folder is scanned, for anything events missed.
const FULL_SCAN: Duration = Duration::from_secs(10 * 60);
/// How often everything is scanned when the folder cannot be watched.
const UNWATCHED_SCAN: Duration = Duration::from_secs(60);
/// How often failed paths and downloads are tried again.
const RETRY: Duration = Duration::from_secs(30);
const SAVE: Duration = Duration::from_secs(5);
/// Downloads in flight at once.
const PARALLEL_FETCHES: usize = 8;
/// How long an apply round waits for more downloads to finish, so a burst
/// of small files is placed in a few rounds rather than one each.
const GATHER: Duration = Duration::from_millis(250);

/// What the watcher reports.
enum Event {
    Paths(Vec<String>),
    Git,
    Everything,
}

/// What the next scan covers.
#[derive(Default)]
struct Due {
    all: bool,
    paths: BTreeSet<String>,
    git: bool,
    since: Option<Instant>,
    last: Option<Instant>,
}

impl Due {
    fn add(&mut self, event: Event) {
        match event {
            Event::Paths(paths) => self.paths.extend(paths),
            Event::Git => self.git = true,
            Event::Everything => self.all = true,
        }
        let now = Instant::now();
        self.since.get_or_insert(now);
        self.last = Some(now);
    }

    fn deadline(&self) -> Option<Instant> {
        Some((self.last? + SETTLE).min(self.since? + MAX_SETTLE))
    }
}

/// Keeps `folder` in step with the devices `peers` names, until dropped.
pub async fn run(node: Arc<Node>, folder: Arc<Folder>, mut peers: watch::Receiver<Vec<DeviceId>>) {
    let (events_tx, mut events) = mpsc::unbounded_channel();
    let watcher = watch_folder(folder.root(), events_tx);
    let full_every = if watcher.is_some() {
        FULL_SCAN
    } else {
        UNWATCHED_SCAN
    };
    let mut full_scan = interval(full_every);
    let mut retry = interval(RETRY);
    let mut save = interval(SAVE);
    let mut due = Due::default();
    let mut pullers: HashMap<DeviceId, AbortOnDropHandle<()>> = HashMap::new();
    let mut fetches: JoinSet<(Hash, anyhow::Result<()>)> = JoinSet::new();
    let mut in_flight: HashSet<Hash> = HashSet::new();
    let mut queued: Vec<(Hash, String)> = Vec::new();
    // When the next apply round runs; rounds gather what arrives meanwhile.
    let mut apply_at: Option<Instant> = None;
    // Paths that could not be read, looked at again on the retry timer.
    let mut unsettled: BTreeSet<String> = BTreeSet::new();

    // The first tick of an interval is immediate: it is the scan at start.
    retry.tick().await;
    save.tick().await;
    follow(&node, &folder, &peers.borrow_and_update(), &mut pullers);

    loop {
        tokio::select! {
            Some(event) = events.recv() => due.add(event),
            () = sleep_until(due.deadline().unwrap_or_else(far_future)), if due.deadline().is_some() => {
                let scope = if due.all { Scope::All } else { Scope::Paths(due.paths.iter().cloned().collect()) };
                let paths_changed = due.all || !due.paths.is_empty();
                let git = due.git;
                due = Due::default();
                if paths_changed {
                    scan(&folder, scope, &mut unsettled).await;
                } else if git {
                    folder.observe_git().await;
                }
            }
            _ = full_scan.tick() => scan(&folder, Scope::All, &mut unsettled).await,
            () = folder.work.notified() => {
                apply_at.get_or_insert_with(|| Instant::now() + GATHER);
            }
            () = sleep_until(apply_at.unwrap_or_else(far_future)), if apply_at.is_some() => {
                apply_at = None;
                apply(&folder, &mut queued).await;
            }
            Some(joined) = fetches.join_next() => {
                if let Ok((hash, result)) = joined {
                    in_flight.remove(&hash);
                    match result {
                        Ok(()) => folder.work.notify_one(),
                        Err(e) => tracing::debug!("{}: fetching {}: {e:#}", folder.id, hash.short()),
                    }
                }
            }
            _ = retry.tick() => {
                if !unsettled.is_empty() {
                    due.add(Event::Paths(std::mem::take(&mut unsettled).into_iter().collect()));
                }
                folder.drop_unwanted_downloads();
                folder.work.notify_one();
            }
            _ = save.tick() => {
                if let Err(e) = folder.save().await {
                    tracing::warn!("{}: saving: {e:#}", folder.id);
                }
            }
            changed = peers.changed() => {
                if changed.is_err() {
                    break;
                }
                follow(&node, &folder, &peers.borrow_and_update(), &mut pullers);
            }
        }

        // Start downloads for what the last round could not place.
        let holders: Vec<DeviceId> = peers.borrow().clone();
        while in_flight.len() < PARALLEL_FETCHES
            && let Some((hash, path)) = queued.pop()
        {
            if holders.is_empty() || !in_flight.insert(hash) {
                continue;
            }
            let (node, folder, holders) = (node.clone(), folder.clone(), ordered(&node, &holders));
            fetches.spawn(async move {
                let result = fetch::content(&node, &folder, &holders, hash, &path).await;
                (hash, result)
            });
        }
    }

    drop(watcher);
    if let Err(e) = folder.save().await {
        tracing::warn!("{}: saving: {e:#}", folder.id);
    }
}

/// Scans `scope`. Paths that could not be read (being written, or not
/// readable) join `unsettled`, for the retry timer: a file that stays
/// unreadable is not scanned over and over.
async fn scan(folder: &Arc<Folder>, scope: Scope, unsettled: &mut BTreeSet<String>) {
    match folder.scan(scope).await {
        Ok(again) => unsettled.extend(again),
        Err(e) => tracing::warn!("{}: {e:#}", folder.id),
    }
}

async fn apply(folder: &Arc<Folder>, queued: &mut Vec<(Hash, String)>) {
    match folder.apply().await {
        Ok(round) => {
            if round.done > 0 {
                tracing::debug!("{}: {} paths brought in line", folder.id, round.done);
            }
            *queued = round.fetch;
        }
        Err(e) => tracing::warn!("{}: {e:#}", folder.id),
    }
}

/// Starts a puller for every peer in `peers` and stops the others.
fn follow(
    node: &Arc<Node>,
    folder: &Arc<Folder>,
    peers: &[DeviceId],
    pullers: &mut HashMap<DeviceId, AbortOnDropHandle<()>>,
) {
    pullers.retain(|peer, _| peers.contains(peer));

    for peer in peers {
        pullers.entry(*peer).or_insert_with(|| {
            AbortOnDropHandle::new(tokio::spawn(pull::follow(
                node.clone(),
                folder.clone(),
                *peer,
            )))
        });
    }
}

/// Connected peers first.
fn ordered(node: &Node, peers: &[DeviceId]) -> Vec<DeviceId> {
    let mut peers = peers.to_vec();
    peers.sort_by_key(|peer| !node.is_connected(peer));
    peers
}

fn far_future() -> Instant {
    Instant::now() + Duration::from_secs(365 * 24 * 3600)
}

/// Watches `root`, sending what changed. `None` when the folder cannot be
/// watched (too many directories for the system's limit, say); it is then
/// scanned on a timer.
fn watch_folder(
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
