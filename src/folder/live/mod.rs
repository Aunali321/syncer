//! A folder being synced: its state in memory and on disk, and the
//! operations that touch its files and repository. One lock serializes
//! everything that touches the disk or the repository, so a scan never sees
//! a half-applied change and a placement never races a scan.

mod apply;
mod content;
mod git_sync;
mod jobs;
mod peers;
mod scan_disk;
mod status;

use content::clean_tmp;

pub use apply::Round;
pub use content::Content;
pub use status::{GitNotes, Missing, Problems, Split, Stopped, Summary};

use crate::{
    folder::{
        MARKER, OWN_DIR, disk,
        index::Replica,
        rules::{Rules, Tracked},
        state::State,
    },
    git::{GitState, Repo},
    ids::{DeviceId, FolderId},
    util::{lock, write_atomic},
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{Notify, watch},
    task::spawn_blocking,
};

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

pub struct Folder {
    pub id: FolderId,
    root: PathBuf,
    state_path: PathBuf,
    me: [u8; 32],
    settings: Settings,
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
    /// Peers whose whole index this device has read since it started.
    caught_up: Mutex<BTreeSet<DeviceId>>,
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
        // A state file means this device synced the folder before: then an
        // empty or missing directory is trouble, not a fresh start. A damaged
        // one starts over from the disk, as a reset does: what is there is
        // recorded as new and meets the peers' entries as concurrent edits,
        // a conflict copy where they differ and never a deletion.
        let fresh = || State::new(rand::random::<Replica>());
        let (mut state, synced_before) = match fs::read(state_path) {
            Ok(bytes) => match State::decode(&bytes) {
                Ok(state) => (state, true),
                Err(e) => {
                    tracing::warn!(
                        "{id}: the saved state is damaged, starting over from the disk: {e:#}"
                    );
                    (fresh(), true)
                }
            },
            Err(e) if e.kind() == ErrorKind::NotFound => (fresh(), false),
            Err(e) => return Err(e).with_context(|| format!("reading {}", state_path.display())),
        };

        check_root(&id, root, synced_before)?;
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

        state.disk.set_folding(disk::Folding::probe(&own.join(TMP)));
        let seq = state.index.seq();
        let rules = load_rules(root, settings, &Arc::default());

        Ok(Arc::new(Self {
            id,
            root: root.to_owned(),
            state_path: state_path.to_owned(),
            me,
            settings,
            state: Mutex::new(state),
            rules: Mutex::new(rules),
            dirty: AtomicBool::new(false),
            seq: watch::Sender::new(seq),
            git: watch::Sender::new(None),
            work: Notify::new(),
            disk_lock: tokio::sync::Mutex::new(()),
            problems: Mutex::default(),
            caught_up: Mutex::default(),
        }))
    }

    /// Checks the folder is still there and still this folder, noting why
    /// not for the status.
    fn check(&self) -> Result<()> {
        let checked = check_root(&self.id, &self.root, true);
        lock(&self.problems).folder = checked.as_ref().err().map(Stopped::from);
        checked
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
        match fs::read(state_path).map(|bytes| State::decode(&bytes)) {
            Ok(Ok(mut state)) => {
                state.forget_disk();
                write_atomic(state_path, &state.encode()?)?;
            }
            // Damaged: opening the folder starts it over from the disk.
            Ok(Err(_)) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("reading {}", state_path.display())),
        }

        fs::create_dir_all(root.join(OWN_DIR))?;
        write_atomic(&root.join(OWN_DIR).join(MARKER), id.as_str().as_bytes())
    }

    /// Reads the rules again, keeping what HEAD tracks unless HEAD moved.
    async fn reload_rules(self: &Arc<Self>) -> Result<Arc<Rules>> {
        let (root, settings) = (self.root.clone(), self.settings);
        let last = lock(&self.rules).tracked().clone();
        let rules = spawn_blocking(move || load_rules(&root, settings, &last)).await?;
        *lock(&self.rules) = rules.clone();
        Ok(rules)
    }
}

/// The folder's rules as its ignore files and checked-out commit say now.
/// What HEAD tracks is read again only when HEAD moved since `last`. Blocks.
fn load_rules(root: &Path, settings: Settings, last: &Arc<Tracked>) -> Arc<Rules> {
    let tracked = match Repo::open(root).filter(|_| settings.use_gitignore) {
        None => Arc::default(),
        Some(repo) => match repo.head_commit() {
            Ok(head) if head.as_ref() == last.head() => last.clone(),
            Ok(head) => match repo.tracked() {
                Ok(paths) => Arc::new(Tracked::new(head, paths)),
                Err(e) => {
                    tracing::warn!("{}: listing tracked files: {e:#}", root.display());
                    Arc::default()
                }
            },
            Err(e) => {
                tracing::warn!("{}: reading HEAD: {e:#}", root.display());
                Arc::default()
            }
        },
    };
    Arc::new(Rules::new(root, settings.use_gitignore, tracked))
}

/// Checks that `root` is this folder: it exists and carries the marker. A
/// folder new to this device gets the directory and the marker made.
fn check_root(id: &FolderId, root: &Path, synced_before: bool) -> Result<()> {
    let marker = root.join(OWN_DIR).join(MARKER);

    match fs::read_to_string(&marker) {
        Ok(text) if text.trim() == id.as_str() => Ok(()),
        Ok(text) => bail!(
            "{} belongs to another folder ({}). Nothing was touched.",
            root.display(),
            text.trim()
        ),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            if synced_before {
                return Err(Missing.into());
            }
            fs::create_dir_all(root.join(OWN_DIR))
                .with_context(|| format!("making {}", root.display()))?;
            write_atomic(&marker, id.as_str().as_bytes())
        }
        Err(e) => Err(e).with_context(|| format!("reading {}", marker.display())),
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::folder::{
        index::{FileMeta, Hash, Kind},
        scan::Scope,
    };

    pub fn folder(root: &Path) -> Arc<Folder> {
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

    pub fn meta(text: &str, mtime_ms: i64) -> FileMeta {
        FileMeta {
            kind: Kind::File,
            hash: Hash::of(text.as_bytes()),
            size: text.len() as u64,
            mtime_ms,
            executable: false,
        }
    }

    /// Makes `text` downloaded content, as a fetch would.
    pub fn download(folder: &Folder, text: &str) {
        let (_, done) = folder.download_paths(&Hash::of(text.as_bytes()));
        fs::write(done, text).expect("write");
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
