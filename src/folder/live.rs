//! A folder being synced: its state in memory and on disk, and the
//! operations that touch its files and repository. One lock serializes
//! everything that touches the disk or the repository, so a scan never sees
//! a half-applied change and a placement never races a scan.

use crate::{
    folder::{
        OWN_DIR, disk,
        index::{Entry, FileMeta, Hash, Kind, Replica, Version},
        rules::{Rules, is_rule_file},
        scan::{self, Scope},
        state::{Cursor, State, Work},
    },
    git::{self, GitState, Repo},
    ids::{DeviceId, FolderId},
    util::{lock, write_atomic},
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime},
};
use tokio::{
    sync::{Notify, watch},
    task::spawn_blocking,
};

/// The marker file in a folder root, holding the folder id.
const MARKER: &str = "folder";
/// Temporary and partial files, on the folder's own file system.
const TMP: &str = "tmp";
/// Completed downloads nobody placed are dropped after this long.
const TMP_AGE: Duration = Duration::from_secs(24 * 3600);

/// Per-folder settings every device shares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// Whether `.gitignore` files apply.
    pub use_gitignore: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            use_gitignore: true,
        }
    }
}

/// Where a folder stands, for `syncer status`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Problems {
    /// Paths that could not be brought in line, with why.
    pub files: BTreeMap<String, String>,
    /// Notes from Git sync with each peer: diverged branches, operations
    /// waited for.
    pub git: BTreeMap<DeviceId, Vec<String>>,
    /// Peers whose changes are held back, with why.
    pub waiting: BTreeMap<DeviceId, String>,
}

/// What an apply round did.
#[derive(Debug, Default)]
pub struct Round {
    /// Paths brought in line.
    pub done: usize,
    /// Content to fetch from peers: (hash, a path that wants it).
    pub fetch: Vec<(Hash, String)>,
    /// Paths that failed and wait for the next round.
    pub failed: usize,
    /// An ignore file was written or removed.
    pub rules_changed: bool,
}

/// Content to send to a peer.
pub enum Content {
    File { path: PathBuf, stat: disk::Stat },
    Bytes(Vec<u8>),
}

pub struct Folder {
    pub id: FolderId,
    root: PathBuf,
    state_path: PathBuf,
    me: [u8; 32],
    settings: Settings,
    case_insensitive: bool,
    state: Mutex<State>,
    rules: Mutex<Arc<Rules>>,
    dirty: AtomicBool,
    /// The index's sequence number: pulls wake when it moves.
    seq: watch::Sender<u64>,
    /// What this device advertises about its repository; `None` when the
    /// folder is not a Git checkout.
    git: watch::Sender<Option<GitState>>,
    /// Wakes the worker when the index changed or content arrived.
    pub work: Notify,
    disk_lock: tokio::sync::Mutex<()>,
    problems: Mutex<Problems>,
}

impl Folder {
    /// Opens the folder at `root`, keeping its state in `state_path`. A
    /// folder this device synced before must still carry its marker: a
    /// missing or foreign marker means an unmounted disk or a replaced
    /// directory, and nothing is touched.
    pub fn open(
        id: FolderId,
        root: &Path,
        state_path: &Path,
        me: [u8; 32],
        settings: Settings,
    ) -> Result<Arc<Self>> {
        let state = match fs::read(state_path) {
            Ok(bytes) => State::decode(&bytes)
                .with_context(|| format!("reading {}", state_path.display()))?,
            Err(e) if e.kind() == ErrorKind::NotFound => State::new(rand::random::<Replica>()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", state_path.display())),
        };

        check_root(&id, root, state.has_history())?;
        if root.join(".git").is_file() {
            bail!(
                "{} is a linked worktree or a submodule; sync its main checkout instead",
                root.display()
            );
        }
        let own = root.join(OWN_DIR);
        fs::create_dir_all(own.join(TMP)).with_context(|| format!("making {}", own.display()))?;
        clean_tmp(&own.join(TMP));

        if let Some(repo) = Repo::open(root) {
            repo.exclude(&format!("/{OWN_DIR}/"))?;
        }

        let case_insensitive = probe_case(&own);
        let seq = state.index.seq();
        let rules = Arc::new(Rules::new(root, settings.use_gitignore));

        Ok(Arc::new(Self {
            id,
            root: root.to_owned(),
            state_path: state_path.to_owned(),
            me,
            settings,
            case_insensitive,
            state: Mutex::new(state),
            rules: Mutex::new(rules),
            dirty: AtomicBool::new(false),
            seq: watch::Sender::new(seq),
            git: watch::Sender::new(None),
            work: Notify::new(),
            disk_lock: tokio::sync::Mutex::new(()),
            problems: Mutex::default(),
        }))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn settings(&self) -> Settings {
        self.settings
    }

    pub fn replica(&self) -> Replica {
        lock(&self.state).index.replica()
    }

    fn changed(&self) {
        self.dirty.store(true, Ordering::Release);
        let seq = lock(&self.state).index.seq();
        self.seq.send_if_modified(|current| {
            let moved = *current != seq;
            *current = seq;
            moved
        });
    }

    /// Saves the state if it changed since the last save.
    pub async fn save(self: &Arc<Self>) -> Result<()> {
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return Ok(());
        }

        let bytes = match lock(&self.state).encode() {
            Ok(bytes) => bytes,
            Err(e) => {
                self.dirty.store(true, Ordering::Release);
                return Err(e);
            }
        };
        let path = self.state_path.clone();
        let saved = spawn_blocking(move || write_atomic(&path, &bytes)).await?;

        if saved.is_err() {
            self.dirty.store(true, Ordering::Release);
        }

        saved
    }

    /// Forgets what was on disk and starts over from a fresh scan that
    /// deletes nothing. For a folder whose marker went missing.
    pub fn reset(root: &Path, id: &FolderId, state_path: &Path) -> Result<()> {
        if let Ok(bytes) = fs::read(state_path) {
            let mut state = State::decode(&bytes)?;
            state.forget_disk();
            write_atomic(state_path, &state.encode()?)?;
        }

        fs::create_dir_all(root.join(OWN_DIR))?;
        write_atomic(&root.join(OWN_DIR).join(MARKER), id.as_str().as_bytes())
    }

    // Scanning.

    /// Scans `scope`, records what changed, then looks at the repository.
    /// Returns paths that were changing while read, to look at again soon.
    pub async fn scan(self: &Arc<Self>, scope: Scope) -> Result<Vec<String>> {
        let _disk = self.disk_lock.lock().await;
        check_root(&self.id, &self.root, true)?;

        let root = self.root.clone();
        let settings = self.settings;
        let rules =
            spawn_blocking(move || Arc::new(Rules::new(&root, settings.use_gitignore))).await?;
        *lock(&self.rules) = rules.clone();

        let walked = {
            let (root, rules, scope) = (self.root.clone(), rules.clone(), scope.clone());
            spawn_blocking(move || scan::walk(&root, &rules, &scope)).await?
        };
        let plan = lock(&self.state).plan_scan(&walked, &scope, &rules);
        let root = self.root.clone();
        let read = spawn_blocking(move || {
            plan.read
                .into_iter()
                .map(|to_read| {
                    let meta = disk::describe(&disk::join(&root, &to_read.path), &to_read.stat);
                    (to_read, meta)
                })
                .collect::<Vec<_>>()
        })
        .await?;

        let rules_changed = read.iter().any(|(to_read, _)| is_rule_file(&to_read.path))
            || plan.gone.iter().any(|gone| is_rule_file(&gone.path));
        let again = {
            let mut state = lock(&self.state);
            let again = state.apply_scan(read, plan.gone, plan.excluded);
            if rules_changed {
                // Paths the old rules left out may sync now, and the reverse.
                state.pend_all();
            }
            again
        };
        // Git first: batches carry the commit the checkout is at, so it must
        // be read before the new entries are announced.
        self.observe_git_locked().await;
        self.changed();
        self.work.notify_one();
        Ok(again)
    }

    /// Looks at the repository for branches, tags and HEAD the user moved.
    pub async fn observe_git(self: &Arc<Self>) {
        let _disk = self.disk_lock.lock().await;
        self.observe_git_locked().await;
    }

    async fn observe_git_locked(self: &Arc<Self>) {
        let Some(repo) = Repo::open(&self.root) else {
            self.git.send_if_modified(|state| state.take().is_some());
            return;
        };

        let mut memory = lock(&self.state).git.clone();
        let me = self.me;
        let observed = spawn_blocking(move || {
            git::sync::observe(&repo, &mut memory, me)?;
            let state = git::sync::advertise(&repo, &memory)?;
            anyhow::Ok((memory, state))
        })
        .await;

        match observed {
            Ok(Ok((memory, state))) => self.set_git(memory, state),
            Ok(Err(e)) => tracing::warn!("{}: reading the repository: {e:#}", self.id),
            Err(e) => tracing::warn!("{}: reading the repository: {e}", self.id),
        }
    }

    fn set_git(&self, memory: git::Memory, state: GitState) {
        {
            let mut current = lock(&self.state);
            if current.git != memory {
                current.git = memory;
                self.dirty.store(true, Ordering::Release);
            }
        }
        self.git.send_if_modified(|current| {
            let moved = current.as_ref() != Some(&state);
            *current = Some(state);
            moved
        });
    }

    // Git alignment with a peer.

    pub fn git_state(&self) -> watch::Receiver<Option<GitState>> {
        self.git.subscribe()
    }

    pub fn head_commit(&self) -> Option<git::Oid> {
        self.git
            .borrow()
            .as_ref()
            .and_then(|state| state.head_commit.clone())
    }

    /// Settles a peer's Git state here. `fetch_pack` brings the objects it
    /// needs into the repository at the folder root: `(wants, haves)`.
    pub async fn align_git<F, Fut>(
        self: &Arc<Self>,
        peer: &DeviceId,
        theirs: &GitState,
        fetch_pack: F,
    ) -> Result<()>
    where
        F: FnOnce(Vec<git::Oid>, Vec<git::Oid>) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        let _disk = self.disk_lock.lock().await;
        let root = self.root.clone();
        let peer_state = theirs.clone();
        let (repo, wants, haves) = spawn_blocking(move || {
            let repo = match Repo::open(&root) {
                Some(repo) => repo,
                None if fs::symlink_metadata(root.join(".git")).is_ok() => {
                    bail!("the folder's .git is not a repository directory here")
                }
                None if peer_state.head.is_some() => git::sync::bootstrap(&root, &peer_state)?,
                None => bail!("the peer's folder has no repository"),
            };
            let (wants, haves) = git::sync::wants(&repo, &peer_state)?;
            anyhow::Ok((repo, wants, haves))
        })
        .await??;

        if !wants.is_empty() {
            fetch_pack(wants, haves)
                .await
                .context("fetching Git objects")?;
        }

        let mut memory = lock(&self.state).git.clone();
        let (label, me, theirs) = (git::sync::label(peer.as_bytes()), self.me, theirs.clone());
        let (memory, state, aligned) = spawn_blocking(move || {
            git::sync::observe(&repo, &mut memory, me)?;
            let aligned = git::sync::align(&repo, &mut memory, &theirs, &label)?;
            let state = git::sync::advertise(&repo, &memory)?;
            anyhow::Ok((memory, state, aligned))
        })
        .await??;

        let notes: Vec<String> = aligned
            .diverged
            .iter()
            .map(|diverged| {
                format!(
                    "branch {} diverged; merge it with: git merge {}",
                    diverged.branch, diverged.reference
                )
            })
            .chain(aligned.notes.iter().cloned())
            .collect();
        {
            let mut problems = lock(&self.problems);
            if notes.is_empty() {
                problems.git.remove(peer);
            } else {
                problems.git.insert(*peer, notes);
            }
        }

        if aligned.checkout_moved {
            tracing::info!("{}: checkout now at {:?}", self.id, state.head_commit);
        }

        self.set_git(memory, state);
        Ok(())
    }

    // Pulling.

    pub fn seq(&self) -> watch::Receiver<u64> {
        self.seq.subscribe()
    }

    /// The entries changed after `since`, and the cursor after them.
    pub fn since(&self, since: u64, limit: usize) -> (Vec<Entry>, u64) {
        lock(&self.state).index.since(since, limit)
    }

    pub fn cursor(&self, peer: &DeviceId) -> Cursor {
        lock(&self.state)
            .cursors
            .get(peer)
            .copied()
            .unwrap_or_default()
    }

    /// Whether a peer's changes, made while its checkout was at `head`, can
    /// land here: only when both checkouts are at the same commit, so two
    /// histories' work trees never mix.
    pub fn accepts(&self, head: Option<&git::Oid>) -> bool {
        self.head_commit().as_ref() == head
    }

    /// Merges entries a peer sent and moves its cursor.
    pub fn merge(&self, peer: &DeviceId, entries: &[Entry], cursor: Cursor) {
        let valid: Vec<Entry> = entries
            .iter()
            .filter(|entry| crate::folder::index::valid_path(&entry.path))
            .cloned()
            .collect();
        {
            let mut state = lock(&self.state);
            state.merge(&valid);
            state.cursors.insert(*peer, cursor);
        }
        self.changed();
        self.work.notify_one();
    }

    pub fn set_waiting(&self, peer: &DeviceId, why: Option<String>) {
        let mut problems = lock(&self.problems);
        match why {
            Some(why) => problems.waiting.insert(*peer, why),
            None => problems.waiting.remove(peer),
        };
    }

    // Applying.

    fn tmp(&self) -> PathBuf {
        self.root.join(OWN_DIR).join(TMP)
    }

    /// Where content with `hash` is downloaded to, and where it is once
    /// verified.
    pub fn download_paths(&self, hash: &Hash) -> (PathBuf, PathBuf) {
        let done = self.tmp().join(hash.to_string());
        (done.with_extension("part"), done)
    }

    /// Brings every pending path in line that can be with content at hand,
    /// and lists the content to fetch.
    pub async fn apply(self: &Arc<Self>) -> Result<Round> {
        let _disk = self.disk_lock.lock().await;
        check_root(&self.id, &self.root, true)?;
        let rules = lock(&self.rules).clone();
        let mut round = Round::default();
        let mut jobs = Vec::new();

        {
            let mut state = lock(&self.state);
            let pending: Vec<String> = state.pending().iter().cloned().collect();

            for path in pending {
                let planned = state.index.get(&path).map(|entry| entry.version.clone());
                match state.work(&path, &rules) {
                    Work::Done => {
                        state.settle(&path, planned.as_ref());
                        lock(&self.problems).files.remove(&path);
                    }
                    Work::Adopt(version) => {
                        state.adopted(&path, version);
                        state.settle(&path, planned.as_ref());
                        round.done += 1;
                    }
                    Work::Place {
                        entry,
                        meta,
                        expected,
                    } => {
                        if expected.is_none()
                            && self.case_insensitive
                            && let Some(other) = state.disk.case_clash(&path)
                        {
                            let why = format!(
                                "{other} is already here, and this file system does not tell the two names apart"
                            );
                            lock(&self.problems).files.insert(path, why);
                            round.failed += 1;
                            continue;
                        }
                        let source = state.disk.holding(&meta.hash, &path).map(|(at, file)| {
                            (disk::join(&self.root, at), file.stat, file.meta.kind)
                        });
                        jobs.push(Job::Place {
                            path,
                            version: entry.version,
                            meta,
                            expected,
                            source,
                        });
                    }
                    Work::Remove { version, expected } => jobs.push(Job::Remove {
                        path,
                        version,
                        expected,
                    }),
                }
            }
        }

        let (root, tmp) = (self.root.clone(), self.tmp());
        let outcomes = spawn_blocking(move || run_jobs(&root, &tmp, jobs)).await?;
        let again = {
            let mut state = lock(&self.state);
            let mut problems = lock(&self.problems);

            for (path, outcome) in outcomes {
                match outcome {
                    Outcome::Placed {
                        meta,
                        stat,
                        version,
                    } => {
                        state.placed(&path, meta, stat, version.clone());
                        state.settle(&path, Some(&version));
                        round.done += 1;
                        round.rules_changed |= is_rule_file(&path);
                        problems.files.remove(&path);
                    }
                    Outcome::Removed { version } => {
                        state.removed(&path);
                        state.settle(&path, Some(&version));
                        round.done += 1;
                        round.rules_changed |= is_rule_file(&path);
                        problems.files.remove(&path);
                    }
                    Outcome::Missing(hash) => round.fetch.push((hash, path)),
                    Outcome::Failed(why) => {
                        round.failed += 1;
                        problems.files.insert(path, why);
                    }
                }
            }

            // Paths whose index entry moved while the round ran are still pending.
            state.pending().iter().any(|path| {
                !round.fetch.iter().any(|(_, wanted)| wanted == path)
                    && !problems.files.contains_key(path)
            })
        };

        if round.done > 0 {
            self.dirty.store(true, Ordering::Release);
        }

        if round.rules_changed {
            let (root, use_gitignore) = (self.root.clone(), self.settings.use_gitignore);
            let rules = spawn_blocking(move || Arc::new(Rules::new(&root, use_gitignore))).await?;
            *lock(&self.rules) = rules;
            lock(&self.state).pend_all();
        }

        if round.rules_changed || (again && round.done > 0) {
            self.work.notify_one();
        }

        round.fetch.sort();
        round.fetch.dedup_by(|a, b| a.0 == b.0);
        Ok(round)
    }

    /// Drops downloaded content once no pending path wants it.
    pub fn drop_unwanted_downloads(&self) {
        let wanted: std::collections::HashSet<Hash> = {
            let state = lock(&self.state);
            state
                .pending()
                .iter()
                .filter_map(|path| state.index.get(path)?.file.as_ref().map(|file| file.hash))
                .collect()
        };
        let Ok(entries) = fs::read_dir(self.tmp()) else {
            return;
        };

        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.len() == 64
                && let Ok(bytes) = hex::decode(name)
                && let Ok(hash) = <[u8; 32]>::try_from(bytes.as_slice())
                && !wanted.contains(&Hash(hash))
            {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    // Serving.

    /// Content with `hash`, preferably at `path`, if it is still on disk as
    /// last seen.
    pub fn content(&self, hash: &Hash, path: &str) -> Result<Option<Content>> {
        let found = lock(&self.state)
            .disk
            .holding(hash, path)
            .map(|(at, file)| (disk::join(&self.root, at), file.stat));
        let Some((at, stat)) = found else {
            // Verified content waiting to be placed is as good.
            let (_, staged) = self.download_paths(hash);
            return Ok(fs::symlink_metadata(&staged)
                .ok()
                .and_then(|meta| disk::Stat::of(&meta))
                .map(|stat| Content::File { path: staged, stat }));
        };

        if !disk::unchanged(&at, Some(&stat))? {
            return Ok(None);
        }

        Ok(Some(match stat.kind {
            Kind::File => Content::File { path: at, stat },
            Kind::Symlink => Content::Bytes(disk::link_bytes(&at)?),
        }))
    }

    // Status.

    pub fn problems(&self) -> Problems {
        lock(&self.problems).clone()
    }

    pub fn summary(&self) -> Summary {
        let state = lock(&self.state);
        let (files, bytes) = state.totals();
        let rules = lock(&self.rules).clone();
        let pending = state
            .pending()
            .iter()
            .filter(|path| !matches!(state.work(path, &rules), Work::Done))
            .count();
        Summary {
            files,
            bytes,
            pending,
            conflicts: state.conflicts(),
            head: self
                .git
                .borrow()
                .as_ref()
                .and_then(|git| git.head.as_ref().map(|head| head.target.clone())),
            head_commit: self.head_commit(),
        }
    }
}

/// Counts for `syncer status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub files: u64,
    pub bytes: u64,
    pub pending: usize,
    pub conflicts: Vec<String>,
    pub head: Option<git::HeadTarget>,
    pub head_commit: Option<git::Oid>,
}

/// A change to the disk, worked out under the state lock and carried out
/// without it.
enum Job {
    Place {
        path: String,
        version: Version,
        meta: FileMeta,
        expected: Option<disk::Stat>,
        /// A file here with the same content, to copy from.
        source: Option<(PathBuf, disk::Stat, Kind)>,
    },
    Remove {
        path: String,
        version: Version,
        expected: disk::Stat,
    },
}

enum Outcome {
    Placed {
        meta: FileMeta,
        stat: disk::Stat,
        version: Version,
    },
    Removed {
        version: Version,
    },
    /// The content is not here: fetch it.
    Missing(Hash),
    Failed(String),
}

/// Carries out a round's jobs. Every placement's content is staged in `tmp`
/// before any file changes, so content one path still needs is never lost to
/// another path's overwrite (a conflict copy and the file it was taken from,
/// say). Removals go last, so a renamed file is copied before its old path
/// goes. Everything here blocks.
fn run_jobs(root: &Path, tmp: &Path, jobs: Vec<Job>) -> Vec<(String, Outcome)> {
    let mut uses: std::collections::HashMap<Hash, usize> = std::collections::HashMap::new();

    for job in &jobs {
        if let Job::Place { meta, source, .. } = job {
            *uses.entry(meta.hash).or_default() += 1;
            let staged = tmp.join(meta.hash.to_string());
            if !staged.exists()
                && let Some((from, stat, kind)) = source
                && let Err(e) = stage(from, stat, *kind, &meta.hash, &staged)
            {
                tracing::debug!("staging {}: {e:#}", from.display());
            }
        }
    }

    let mut outcomes = Vec::with_capacity(jobs.len());
    let mut removals = Vec::new();

    for job in jobs {
        match job {
            Job::Place {
                path,
                version,
                meta,
                expected,
                ..
            } => {
                let staged = tmp.join(meta.hash.to_string());
                let outcome = if staged.exists() {
                    let last = uses.get_mut(&meta.hash).map(|count| {
                        *count -= 1;
                        *count == 0
                    });
                    match place_staged(
                        root,
                        &path,
                        &staged,
                        &meta,
                        expected.as_ref(),
                        last == Some(true),
                    ) {
                        Ok(stat) => Outcome::Placed {
                            meta,
                            stat,
                            version,
                        },
                        Err(e) => Outcome::Failed(format!("{e:#}")),
                    }
                } else {
                    Outcome::Missing(meta.hash)
                };
                outcomes.push((path, outcome));
            }
            Job::Remove {
                path,
                version,
                expected,
            } => removals.push((path, version, expected)),
        }
    }

    for (path, version, expected) in removals {
        let outcome = match disk::remove(root, &path, &expected) {
            Ok(()) => Outcome::Removed { version },
            Err(e) => Outcome::Failed(format!("{e:#}")),
        };
        outcomes.push((path, outcome));
    }

    outcomes
}

/// Copies content from a file here into `staged`, checking its hash.
fn stage(from: &Path, stat: &disk::Stat, kind: Kind, hash: &Hash, staged: &Path) -> Result<()> {
    if !disk::unchanged(from, Some(stat))? {
        bail!("changed since it was scanned");
    }
    let partial = staged.with_extension("stage");
    match kind {
        Kind::File => disk::copy_verified(from, &partial, hash)?,
        Kind::Symlink => {
            let bytes = disk::link_bytes(from)?;
            if Hash::of(&bytes) != *hash {
                bail!("the link changed since it was scanned");
            }
            fs::write(&partial, bytes)?;
        }
    }
    fs::rename(&partial, staged)?;
    Ok(())
}

/// Places staged content at `path`. The staged copy is moved on its last
/// use and copied before that; it stays when placement fails.
fn place_staged(
    root: &Path,
    path: &str,
    staged: &Path,
    meta: &FileMeta,
    expected: Option<&disk::Stat>,
    last_use: bool,
) -> Result<disk::Stat> {
    let content = staged.with_extension("place");

    if meta.kind == Kind::Symlink {
        let target = fs::read(staged)?;
        let _ = fs::remove_file(&content);
        disk::write_symlink(&content, &target)?;
    } else if last_use {
        fs::rename(staged, &content)?;
    } else {
        fs::copy(staged, &content).context("copying staged content")?;
    }

    let placed = disk::place(root, path, &content, meta, expected);
    if placed.is_err() {
        if meta.kind == Kind::File && last_use {
            let _ = fs::rename(&content, staged);
        } else {
            let _ = fs::remove_file(&content);
        }
    } else if meta.kind == Kind::Symlink && last_use {
        let _ = fs::remove_file(staged);
    }
    placed
}

/// Checks that `root` is this folder: it exists and carries the marker. A
/// folder new to this device gets the directory and the marker made.
fn check_root(id: &FolderId, root: &Path, has_history: bool) -> Result<()> {
    let marker = root.join(OWN_DIR).join(MARKER);

    match fs::read_to_string(&marker) {
        Ok(text) if text.trim() == id.as_str() => Ok(()),
        Ok(text) => bail!(
            "{} belongs to another folder ({}); refusing to touch it",
            root.display(),
            text.trim()
        ),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            if has_history {
                bail!(
                    "{} is missing or has lost its {OWN_DIR}/{MARKER} marker. If the disk is \
                     mounted and this is the right directory, run `syncer reset {id}` to start \
                     over without deleting anything",
                    root.display()
                );
            }
            fs::create_dir_all(root.join(OWN_DIR))
                .with_context(|| format!("making {}", root.display()))?;
            write_atomic(&marker, id.as_str().as_bytes())
        }
        Err(e) => Err(e).with_context(|| format!("reading {}", marker.display())),
    }
}

/// Removes leftovers of placements and old finished downloads. Partial
/// downloads stay, to be resumed.
fn clean_tmp(tmp: &Path) {
    let Ok(entries) = fs::read_dir(tmp) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let stale = match path.extension().and_then(|ext| ext.to_str()) {
            Some("place" | "link") => true,
            Some("part") => false,
            _ => entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                .is_some_and(|age| age > TMP_AGE),
        };
        if stale {
            let _ = fs::remove_file(&path);
        }
    }
}

/// Whether the file system at `dir` treats names that differ only by case as
/// the same name.
fn probe_case(dir: &Path) -> bool {
    let probe = dir.join("case-probe");
    if fs::write(&probe, b"").is_err() {
        return false;
    }
    let insensitive = dir.join("CASE-PROBE").exists();
    let _ = fs::remove_file(&probe);
    insensitive
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::folder::index::Version;

    fn folder(root: &Path) -> Arc<Folder> {
        let state = root.join("state");
        Folder::open(
            FolderId::random(),
            &root.join("work"),
            &state,
            [7; 32],
            Settings::default(),
        )
        .expect("the folder opens")
    }

    fn meta(text: &str, mtime_ms: i64) -> FileMeta {
        FileMeta {
            kind: Kind::File,
            hash: Hash::of(text.as_bytes()),
            size: text.len() as u64,
            mtime_ms,
            executable: false,
        }
    }

    /// Makes `text` downloaded content, as a fetch would.
    fn download(folder: &Folder, text: &str) {
        let (_, done) = folder.download_paths(&Hash::of(text.as_bytes()));
        fs::write(done, text).expect("write");
    }

    #[tokio::test]
    async fn a_losing_local_edit_survives_as_its_conflict_copy() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let folder = folder(dir.path());
        // `Makefile` sorts before `Makefile (conflict ...)`: the original is
        // worked on first, and its local content must already be staged.
        fs::write(folder.root().join("Makefile"), "local").expect("write");
        folder.scan(Scope::All).await.expect("scanned");

        let theirs = Entry {
            path: "Makefile".into(),
            version: Version::default().with(99, 1),
            file: Some(meta("theirs", i64::MAX / 2)),
        };
        download(&folder, "theirs");
        folder.merge(
            &iroh::SecretKey::from_bytes(&[1; 32]).public(),
            &[theirs],
            Default::default(),
        );
        folder.apply().await.expect("applied");

        let copy = crate::folder::index::conflict_path("Makefile", &Hash::of(b"local"));
        assert_eq!(
            fs::read_to_string(folder.root().join("Makefile")).expect("read"),
            "theirs"
        );
        assert_eq!(
            fs::read_to_string(folder.root().join(&copy)).expect("the copy"),
            "local"
        );
    }

    #[tokio::test]
    async fn a_path_whose_entry_moves_during_a_round_stays_pending() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let folder = folder(dir.path());
        let peer = iroh::SecretKey::from_bytes(&[1; 32]).public();
        let first = Entry {
            path: "a".into(),
            version: Version::default().with(99, 1),
            file: Some(meta("one", 1)),
        };
        folder.merge(&peer, std::slice::from_ref(&first), Default::default());
        download(&folder, "one");

        // The round places `one`; a newer entry arrives before it settles.
        let round = folder.apply().await.expect("applied");
        assert_eq!(round.done, 1);
        let second = Entry {
            path: "a".into(),
            version: first.version.with(99, 2),
            file: Some(meta("two", 2)),
        };
        lock(&folder.state).settle("a", Some(&first.version));
        folder.merge(&peer, &[second], Default::default());
        lock(&folder.state).settle("a", Some(&first.version));
        assert!(lock(&folder.state).pending().contains("a"));

        download(&folder, "two");
        folder.apply().await.expect("applied");
        assert_eq!(
            fs::read_to_string(folder.root().join("a")).expect("read"),
            "two"
        );
    }

    #[tokio::test]
    async fn a_folder_without_its_marker_is_left_alone() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let folder = folder(dir.path());
        fs::write(folder.root().join("a"), "keep").expect("write");
        folder.scan(Scope::All).await.expect("scanned");
        folder.save().await.expect("saved");
        let id = folder.id.clone();
        drop(folder);

        fs::remove_dir_all(dir.path().join("work")).expect("unmounted");
        fs::create_dir(dir.path().join("work")).expect("an empty mount point");
        let reopened = Folder::open(
            id,
            &dir.path().join("work"),
            &dir.path().join("state"),
            [7; 32],
            Settings::default(),
        );
        assert!(reopened.is_err());
    }
}
