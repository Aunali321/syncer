//! Folder commands: sharing, taking, removing and pausing folders.

use crate::{
    cluster::FolderInfo,
    folder::{Folder, Settings},
    home::LocalFolder,
    ids::FolderId,
    util::lock,
};
use anyhow::{Context, Result, bail, ensure};
use std::path::{Path, PathBuf};

use super::*;

impl Service {
    /// Shares the directory at `path` as a new folder.
    pub fn add_folder(
        &self,
        path: &Path,
        label: Option<String>,
        settings: Settings,
    ) -> Result<FolderId> {
        let path = absolute(path)?;
        ensure!(path.is_dir(), "{} is not a directory", path.display());
        self.check_free(&path)?;
        let label = label
            .or_else(|| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "folder".to_owned());
        let id = FolderId::random();
        let home_path = dirs::home_dir()
            .and_then(|home| {
                path.strip_prefix(home)
                    .ok()?
                    .components()
                    .map(|part| part.as_os_str().to_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
            })
            .filter(|parts| !parts.is_empty())
            .map(|parts| parts.join("/"));

        self.change_cluster(|cluster| {
            cluster.set_folder(
                id.clone(),
                FolderInfo {
                    label,
                    settings,
                    removed: false,
                    home_path,
                },
            );
            cluster.set_holding(&id, true);
        })?;
        self.change_config(|config| {
            config.folders.insert(
                id.clone(),
                LocalFolder {
                    path,
                    paused: false,
                },
            );
        })?;
        Ok(id)
    }

    /// Takes a folder another device offers, into `path`.
    /// Takes a folder another device offers, into `path`, or by default
    /// into the place it has on the device that shared it.
    pub fn accept_folder(&self, id: &FolderId, path: Option<&Path>) -> Result<PathBuf> {
        let info = lock(&self.cluster)
            .folder(id)
            .cloned()
            .with_context(|| format!("No folder named \"{id}\"."))?;
        ensure!(
            !lock(&self.config).folders.contains_key(id),
            "Already syncing {}.",
            info.label
        );
        let path = match path {
            Some(path) => absolute(path)?,
            None => same_place(&info).with_context(|| {
                format!(
                    "Where should {0} go? Run: syncer accept {0} <path>",
                    info.label
                )
            })?,
        };
        if path.exists() {
            ensure!(path.is_dir(), "{} is not a directory", path.display());
        }
        self.check_free(&path)?;

        // Holding first, so the others hear of it before the folder starts
        // and asks them for it.
        self.change_cluster(|cluster| cluster.set_holding(id, true))?;
        self.change_config(|config| {
            config.folders.insert(
                id.clone(),
                LocalFolder {
                    path: path.clone(),
                    paused: false,
                },
            );
        })?;
        Ok(path)
    }

    /// Fails when `path` is a folder this device already holds, or would
    /// split a Git checkout. A folder may sit inside another: the outer one
    /// leaves the inner one out, as it does a nested repository. But a
    /// directory of a checkout is not its own folder: the outer folder would
    /// leave out files its commits track.
    fn check_free(&self, path: &Path) -> Result<()> {
        let splits = |outer: &Path, inner: &Path| {
            inner.starts_with(outer)
                && outer.join(".git").is_dir()
                && std::fs::symlink_metadata(inner.join(".git")).is_err()
        };
        for folder in lock(&self.config).folders.values() {
            ensure!(folder.path != path, "Already syncing that folder.");
            ensure!(
                !splits(&folder.path, path),
                "This folder is inside the Git repo at {}. Share the whole repo instead.",
                folder.path.display()
            );
            ensure!(
                !splits(path, &folder.path),
                "This repo already has a folder inside it at {}. Remove that one first.",
                folder.path.display()
            );
        }
        if let Some(managed) = crate::syncthing::managing(path) {
            bail!("{managed}");
        }
        Ok(())
    }

    /// Stops syncing a folder here; its files stay. With `everywhere`, the
    /// folder leaves the cluster for every device.
    pub fn remove_folder(&self, id: &FolderId, everywhere: bool) -> Result<()> {
        // Stopped here, without the save a stop otherwise makes, so the state
        // removed below does not come back.
        let running = lock(&self.folders).remove(id);
        drop(running);
        self.change_config(|config| {
            config.folders.remove(id);
        })?;
        lock(&self.failed).remove(id);
        let info = lock(&self.cluster).folder(id).cloned();
        self.change_cluster(|cluster| {
            cluster.set_holding(id, false);
            if everywhere && let Some(info) = info {
                cluster.set_folder(
                    id.clone(),
                    FolderInfo {
                        removed: true,
                        ..info
                    },
                );
            }
        })?;
        let _ = std::fs::remove_file(self.home.folder_state(id));
        Ok(())
    }

    pub fn pause(&self, id: &FolderId, paused: bool) -> Result<()> {
        ensure!(
            lock(&self.config).folders.contains_key(id),
            "this device does not hold {id}"
        );
        self.change_config(|config| {
            if let Some(folder) = config.folders.get_mut(id) {
                folder.paused = paused;
            }
        })
    }

    /// Starts a folder over without deleting anything: for a folder whose
    /// marker is gone after a disk was replaced or remounted.
    pub fn reset(&self, id: &FolderId) -> Result<()> {
        let path = lock(&self.config)
            .folders
            .get(id)
            .map(|folder| folder.path.clone())
            .with_context(|| format!("this device does not hold {id}"))?;
        lock(&self.folders).remove(id);
        Folder::reset(&path, id, &self.home.folder_state(id))?;
        lock(&self.failed).remove(id);
        self.config_changes
            .send_modify(|generation| *generation += 1);
        Ok(())
    }

    pub async fn rescan(&self, id: &FolderId) -> Result<()> {
        let folder = self
            .folder(id)
            .with_context(|| format!("{id} is not running here"))?;
        folder.scan(crate::folder::scan::Scope::All).await?;
        Ok(())
    }
}

/// Where `folder` is on the device that shared it, under this device's home.
fn same_place(folder: &FolderInfo) -> Option<PathBuf> {
    let inside = folder.home_path.as_deref()?;
    let parts: Vec<&str> = inside.split('/').collect();
    // From a peer: a plain path inside the home directory, nothing that
    // climbs out of it.
    if parts
        .iter()
        .any(|part| part.is_empty() || *part == "." || *part == ".." || part.contains(['\\', ':']))
    {
        return None;
    }
    let mut path = dirs::home_dir()?;
    path.extend(parts);
    Some(path)
}

/// A directory name for a folder label: the label without path separators
/// or characters some systems refuse, else the folder id.
pub(super) fn safe_name(label: &str, id: &FolderId) -> String {
    let name: String = label
        .chars()
        .map(|c| {
            if c.is_control() || "/\\:*?\"<>|".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let name = name.trim().trim_matches('.');
    if name.is_empty() {
        id.to_string()
    } else {
        name.to_owned()
    }
}

fn absolute(path: &Path) -> Result<PathBuf> {
    let path =
        std::path::absolute(path).with_context(|| format!("resolving {}", path.display()))?;
    Ok(match crate::util::canonical(&path) {
        Some(real) => real,
        None => path,
    })
}
