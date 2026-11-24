//! The task that keeps one folder in step: it watches the disk, scans what
//! changed, applies what peers changed, fetches content, follows each peer
//! that holds the folder and saves the state.

use super::watch::{Event, watch_folder};
use crate::{
    folder::{Folder, index::Hash, scan::Scope},
    ids::DeviceId,
    net::node::Node,
    sync::{fetch, pull},
    util::bytes,
};
use std::{
    collections::{BTreeSet, HashMap},
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
/// How long content must stay missing from every device before conflict
/// copies of it are given up on.
const GIVE_UP: Duration = Duration::from_secs(30);
/// How long an apply round waits for more downloads to finish, so a burst
/// of small files is placed in a few rounds rather than one each.
const GATHER: Duration = Duration::from_millis(250);

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
    let mut fetches: JoinSet<(Hash, String, anyhow::Result<fetch::Fetched>)> = JoinSet::new();
    // Content every peer said it does not have, and since when.
    let mut nowhere: HashMap<Hash, Instant> = HashMap::new();
    // Downloads under way, with the bytes each may still write.
    let mut in_flight: HashMap<Hash, u64> = HashMap::new();
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
                if let Ok((hash, path, result)) = joined {
                    in_flight.remove(&hash);
                    match result {
                        Ok(fetch::Fetched::Done) => {
                            nowhere.remove(&hash);
                            folder.content_arrived(&hash);
                        }
                        // Asked twice, a while apart, of every device that
                        // holds the folder: it exists nowhere.
                        Ok(fetch::Fetched::Nowhere) => match nowhere.get(&hash) {
                            Some(since) if since.elapsed() >= GIVE_UP => {
                                nowhere.remove(&hash);
                                folder.give_up_on(&hash);
                                folder.work.notify_one();
                            }
                            Some(_) => {}
                            None => {
                                nowhere.insert(hash, Instant::now());
                            }
                        },
                        Err(e) => {
                            nowhere.remove(&hash);
                            tracing::debug!("{}: fetching {}: {e:#}", folder.id, hash.short());
                            folder.note_problem(&path, format!("download failed: {}", crate::util::describe(&e)));
                        }
                    }
                }
            }
            _ = retry.tick() => {
                if !unsettled.is_empty() {
                    due.add(Event::Paths(std::mem::take(&mut unsettled).into_iter().collect()));
                }
                folder.drop_unwanted_downloads();
                // Downloads that failed or waited for space go again.
                queued = folder.awaited();
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

        // Start downloads for what the last round could not place, as far as
        // the disk has room for all of them to finish: started ones that
        // share out the last of the space would each stop short.
        let holders: Vec<DeviceId> = peers.borrow().clone();
        while in_flight.len() < PARALLEL_FETCHES
            && let Some((hash, path)) = queued.pop()
        {
            if holders.is_empty() || in_flight.contains_key(&hash) {
                continue;
            }
            let need = folder.still_to_fetch(&hash, &path);
            let promised: u64 = in_flight.values().sum();
            let free = folder
                .free_space()
                .map_or(u64::MAX, |free| free.saturating_sub(promised));
            if need > free {
                folder.note_problem(
                    &path,
                    format!("needs {} more disk space", bytes(need - free)),
                );
                continue;
            }
            in_flight.insert(hash, need);
            let (node, folder, holders) = (node.clone(), folder.clone(), ordered(&node, &holders));
            fetches.spawn(async move {
                let result = fetch::content(&node, &folder, &holders, hash, &path).await;
                (hash, path, result)
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
            queued.extend(round.fetch);
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
