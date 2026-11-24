//! Content: downloads waiting to be placed, and content served to peers.

use crate::{
    folder::{
        OWN_DIR, disk,
        index::{Hash, Kind, Version},
    },
    util::lock,
};
use anyhow::Result;
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

use super::*;

/// Content to send to a peer.
pub enum Content {
    File { path: PathBuf, stat: disk::Stat },
    Bytes(Vec<u8>),
}

impl Folder {
    pub(super) fn tmp(&self) -> PathBuf {
        self.root.join(OWN_DIR).join(TMP)
    }

    /// Where content with `hash` is downloaded to, and where it is once
    /// verified.
    pub fn download_paths(&self, hash: &Hash) -> (PathBuf, PathBuf) {
        let done = self.tmp().join(hash.to_string());
        (done.with_extension("part"), done)
    }

    /// The bytes of content `hash`, wanted at `path`, still to download.
    pub fn still_to_fetch(&self, hash: &Hash, path: &str) -> u64 {
        let size = lock(&self.state)
            .index
            .get(path)
            .and_then(|entry| entry.file.as_ref())
            .filter(|file| file.hash == *hash)
            .map_or(0, |file| file.size);
        let (part, _) = self.download_paths(hash);
        let have = fs::metadata(part).map_or(0, |meta| meta.len());
        size.saturating_sub(have)
    }

    /// Free space where downloads go, for this user.
    pub fn free_space(&self) -> std::io::Result<u64> {
        fs4::available_space(self.tmp())
    }

    /// Notes why `path` could not be brought in line; placing it clears it.
    /// Content `hash` was downloaded: the paths waiting for it go again.
    pub fn content_arrived(&self, hash: &Hash) {
        lock(&self.state).content_arrived(hash);
        self.work.notify_one();
    }

    /// Every path waiting for content, with the content it waits for.
    pub fn awaited(&self) -> Vec<(Hash, String)> {
        lock(&self.state)
            .awaiting()
            .map(|(path, hash)| (*hash, path.to_owned()))
            .collect()
    }

    /// Gives up on conflict copies of content no device has: an older
    /// version that its own device overwrote before anyone fetched it, made
    /// into a copy by a device that only saw it in passing. Nothing can ever
    /// fill such a copy, so it is deleted (everywhere, like any deletion)
    /// instead of staying to do forever. Only conflict copies: a file the
    /// user named waits for its content however long it takes.
    pub fn give_up_on(&self, hash: &Hash) {
        let mut state = lock(&self.state);
        if state.disk.holding(hash, "").is_some() || self.download_paths(hash).1.exists() {
            return;
        }
        let copies: Vec<(String, Version)> = state
            .pending()
            .iter()
            .map(String::as_str)
            .chain(state.awaiting_for(hash))
            .filter(|path| crate::folder::index::is_conflict(path))
            .filter_map(|path| {
                let entry = state.index.get(path)?;
                (entry.file.as_ref()?.hash == *hash)
                    .then(|| (path.to_owned(), entry.version.clone()))
            })
            .collect();

        for (path, version) in copies {
            tracing::info!(
                "{}: no device has the content of {path}; removing it",
                self.id
            );
            let (_, changed) = state.index.edit(&path, &version, None);
            state.pend(changed);
        }
        drop(state);
        self.changed();
    }

    /// Drops downloaded content once no pending path wants it.
    pub fn drop_unwanted_downloads(&self) {
        let wanted: HashSet<Hash> = {
            let state = lock(&self.state);
            state
                .pending()
                .iter()
                .filter_map(|path| state.index.get(path)?.file.as_ref().map(|file| file.hash))
                .chain(state.awaiting().map(|(_, hash)| *hash))
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
}

/// Removes leftovers of placements and old finished downloads. Partial
/// downloads stay, to be resumed.
pub(super) fn clean_tmp(tmp: &Path) {
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
