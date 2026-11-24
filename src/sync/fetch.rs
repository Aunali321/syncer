//! Fetching file content and Git objects from peers.

use crate::{
    folder::{Folder, disk, index::Hash},
    git::{Oid, Repo},
    ids::DeviceId,
    net::{
        node::Node,
        proto::{Chunk, GetReply, Request},
        wire,
    },
};
use anyhow::{Context, Result, bail, ensure};
use iroh::endpoint::Connection;
use std::{
    io::{Read, SeekFrom},
    path::Path,
    sync::Arc,
};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    task::spawn_blocking,
};

/// Fetches content `hash` (wanted at `path`) from the first of `peers` that
/// has it, into the folder's downloads.
/// What fetching content came to.
#[derive(Debug, PartialEq, Eq)]
pub enum Fetched {
    /// The content is in the folder's downloads.
    Done,
    /// Every peer was reached and none has it.
    Nowhere,
}

/// Fetches content `hash` (wanted at `path`) from the first of `peers` that
/// has it, into the folder's downloads. Fails when a peer could not be
/// asked, so `Nowhere` means every peer said it does not have it.
pub async fn content(
    node: &Node,
    folder: &Arc<Folder>,
    peers: &[DeviceId],
    hash: Hash,
    path: &str,
) -> Result<Fetched> {
    let mut failed = None;

    for peer in peers {
        let attempt = async {
            let connection = node.connect(*peer).await?;
            content_from(&connection, folder, hash, path).await
        };
        match attempt.await {
            Ok(true) => return Ok(Fetched::Done),
            Ok(false) => tracing::debug!(
                "{}: {path}: {} does not have it",
                folder.id,
                crate::ids::short(peer)
            ),
            Err(e) => {
                tracing::debug!(
                    "{}: {path} from {}: {e:#}",
                    folder.id,
                    crate::ids::short(peer)
                );
                failed = Some(e);
            }
        }
    }

    match failed {
        Some(e) => Err(e),
        None if peers.is_empty() => Err(anyhow::anyhow!("no peer holds this folder")),
        None => Ok(Fetched::Nowhere),
    }
}

async fn content_from(
    connection: &Connection,
    folder: &Arc<Folder>,
    hash: Hash,
    path: &str,
) -> Result<bool> {
    let (part, done) = folder.download_paths(&hash);
    let offset = tokio::fs::metadata(&part)
        .await
        .map(|meta| meta.len())
        .unwrap_or(0);
    let (mut send, mut recv) = connection.open_bi().await?;
    wire::send(
        &mut send,
        &Request::Get {
            folder: folder.id.clone(),
            hash,
            path: path.to_owned(),
            offset,
        },
    )
    .await?;
    send.finish()?;

    let size = match wire::expect::<GetReply>(&mut recv).await? {
        GetReply::Found { size } => size,
        GetReply::Missing => return Ok(false),
    };

    // The part already here counts towards the hash.
    let mut hasher = {
        let part = part.clone();
        spawn_blocking(move || hash_prefix(&part)).await??
    };
    let mut out = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&part)
        .await
        .with_context(|| format!("writing {}", part.display()))?;
    // Not opened to append: a run of zeros is skipped over, leaving a hole.
    out.seek(SeekFrom::Start(offset)).await?;
    let mut received = 0u64;

    loop {
        match wire::expect::<Chunk>(&mut recv).await? {
            Chunk::Data(bytes) => {
                received += bytes.len() as u64;
                ensure!(received <= size, "the peer sent more than it said");
                hasher.update(&bytes);
                if disk::is_hole(&bytes) {
                    out.seek(SeekFrom::Current(bytes.len() as i64)).await?;
                } else {
                    out.write_all(&bytes).await?;
                }
            }
            Chunk::End => break,
            Chunk::Failed(why) => {
                drop(out);
                // What arrived may be part of another version: start over.
                let _ = tokio::fs::remove_file(&part).await;
                bail!(why);
            }
        }
    }

    out.set_len(offset + received).await?;
    out.sync_all().await?;
    drop(out);

    if Hash::from(hasher.finalize()) != hash {
        let _ = tokio::fs::remove_file(&part).await;
        bail!("the content arrived damaged");
    }

    tokio::fs::rename(&part, &done).await?;
    Ok(true)
}

fn hash_prefix(part: &Path) -> Result<blake3::Hasher> {
    let mut hasher = blake3::Hasher::new();
    let Ok(mut file) = std::fs::File::open(part) else {
        return Ok(hasher);
    };
    let mut buf = vec![0; 256 * 1024];

    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            return Ok(hasher);
        }
        hasher.update(&buf[..read]);
    }
}

/// Fetches a pack of `wants` (without what `haves` reach) from the peer at
/// the other end of `connection` into the folder's repository.
pub async fn pack(
    connection: &Connection,
    folder: &Arc<Folder>,
    wants: Vec<Oid>,
    haves: Vec<Oid>,
) -> Result<()> {
    let repo = Repo::open(folder.root()).context("the folder has no repository")?;
    let (mut send, mut recv) = connection.open_bi().await?;
    wire::send(
        &mut send,
        &Request::Pack {
            folder: folder.id.clone(),
            wants,
            haves,
        },
    )
    .await?;
    send.finish()?;

    let mut child = tokio::process::Command::from(repo.unpack_command())
        .kill_on_drop(true)
        .spawn()
        .context("running git index-pack")?;
    let mut stdin = child.stdin.take().context("git index-pack has no input")?;
    let mut stderr = child
        .stderr
        .take()
        .context("git index-pack has no error output")?;
    let errors = tokio::spawn(async move {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text).await;
        text
    });

    loop {
        match wire::expect::<Chunk>(&mut recv).await? {
            Chunk::Data(bytes) => stdin.write_all(&bytes).await?,
            Chunk::End => break,
            Chunk::Failed(why) => bail!(why),
        }
    }

    drop(stdin);
    let status = child.wait().await?;
    if !status.success() {
        bail!(
            "git index-pack failed: {}",
            errors.await.unwrap_or_default().trim()
        );
    }
    Ok(())
}
