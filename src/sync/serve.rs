//! Answering a peer's requests for a folder: its index, file content and Git
//! objects.

use crate::{
    folder::{
        Folder, disk,
        index::{BATCH, Hash},
        live::Content,
        state::Cursor,
    },
    git::{Oid, Repo},
    net::{
        proto::{CHUNK, Chunk, GetReply, PullMessage},
        wire,
    },
};
use anyhow::{Context, Result};
use iroh::endpoint::SendStream;
use std::sync::Arc;
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    task::spawn_blocking,
};

/// Streams the folder's index after `cursor`, then every change as it
/// happens, until the peer goes away.
pub async fn pull(folder: Arc<Folder>, mut send: SendStream, cursor: Cursor) -> Result<()> {
    let replica = folder.replica();
    let mut since = if cursor.replica == replica {
        cursor.seq
    } else {
        0
    };
    let mut seq = folder.seq();
    let mut git = folder.git_state();
    let mut caught_up = false;

    wire::send(&mut send, &PullMessage::Start { replica }).await?;
    let current = git.borrow_and_update().clone();
    if let Some(state) = current {
        wire::send(&mut send, &PullMessage::Git(state)).await?;
    }

    loop {
        seq.borrow_and_update();
        let (entries, upto) = folder.since(since, BATCH);

        if upto > since {
            let head = folder.head_commit();
            wire::send(
                &mut send,
                &PullMessage::Batch {
                    entries,
                    upto,
                    head,
                },
            )
            .await?;
            since = upto;
            caught_up = false;
            continue;
        }

        if !caught_up {
            let head = folder.head_commit();
            wire::send(
                &mut send,
                &PullMessage::Batch {
                    entries: Vec::new(),
                    upto: since,
                    head,
                },
            )
            .await?;
            caught_up = true;
        }

        tokio::select! {
            changed = seq.changed() => changed?,
            changed = git.changed() => {
                changed?;
                let current = git.borrow_and_update().clone();
                if let Some(state) = current {
                    wire::send(&mut send, &PullMessage::Git(state)).await?;
                }
            }
            _ = send.stopped() => return Ok(()),
        }
    }
}

/// Sends content with `hash` from `offset`, checking at the end that the
/// file did not change while it was read.
pub async fn get(
    folder: Arc<Folder>,
    mut send: SendStream,
    hash: Hash,
    path: String,
    offset: u64,
) -> Result<()> {
    let content = {
        let folder = folder.clone();
        spawn_blocking(move || folder.content(&hash, &path)).await??
    };

    match content {
        None => wire::send(&mut send, &GetReply::Missing).await?,
        Some(Content::Bytes(bytes)) => {
            let rest = bytes
                .get(usize::try_from(offset).unwrap_or(usize::MAX)..)
                .unwrap_or_default();
            wire::send(
                &mut send,
                &GetReply::Found {
                    size: rest.len() as u64,
                },
            )
            .await?;
            wire::send(&mut send, &Chunk::Data(rest.to_vec())).await?;
            wire::send(&mut send, &Chunk::End).await?;
        }
        Some(Content::File { path, stat }) => {
            let mut file = tokio::fs::File::open(&path)
                .await
                .with_context(|| format!("opening {}", path.display()))?;
            let offset = offset.min(stat.size);
            file.seek(std::io::SeekFrom::Start(offset)).await?;
            wire::send(
                &mut send,
                &GetReply::Found {
                    size: stat.size - offset,
                },
            )
            .await?;
            let mut buf = vec![0; CHUNK];

            loop {
                let read = file.read(&mut buf).await?;
                if read == 0 {
                    break;
                }
                wire::send(&mut send, &Chunk::Data(buf[..read].to_vec())).await?;
            }

            let unchanged = spawn_blocking(move || disk::unchanged(&path, Some(&stat))).await??;
            let last = if unchanged {
                Chunk::End
            } else {
                Chunk::Failed("the file changed while it was sent".into())
            };
            wire::send(&mut send, &last).await?;
        }
    }

    send.finish()?;
    send.stopped().await.ok();
    Ok(())
}

/// Sends a pack of `wants` without what `haves` reach.
pub async fn pack(
    folder: Arc<Folder>,
    mut send: SendStream,
    wants: Vec<Oid>,
    haves: Vec<Oid>,
) -> Result<()> {
    let root = folder.root().to_owned();
    let prepared = spawn_blocking(move || {
        let repo = Repo::open(&root).context("this folder has no repository")?;
        repo.pack_command(&wants, &haves)
    })
    .await?;
    let (command, input) = match prepared {
        Ok(prepared) => prepared,
        Err(e) => {
            wire::send(&mut send, &Chunk::Failed(format!("{e:#}"))).await?;
            return Ok(());
        }
    };

    let mut child = tokio::process::Command::from(command)
        .kill_on_drop(true)
        .spawn()
        .context("running git pack-objects")?;
    let mut stdin = child
        .stdin
        .take()
        .context("git pack-objects has no input")?;
    let mut stdout = child
        .stdout
        .take()
        .context("git pack-objects has no output")?;
    let mut stderr = child
        .stderr
        .take()
        .context("git pack-objects has no error output")?;
    let feed = tokio::spawn(async move {
        let _ = stdin.write_all(&input).await;
    });
    let errors = tokio::spawn(async move {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text).await;
        text
    });
    let mut buf = vec![0; CHUNK];

    loop {
        let read = stdout.read(&mut buf).await?;
        if read == 0 {
            break;
        }
        wire::send(&mut send, &Chunk::Data(buf[..read].to_vec())).await?;
    }

    feed.await.ok();
    let status = child.wait().await?;
    let last = if status.success() {
        Chunk::End
    } else {
        Chunk::Failed(format!(
            "git pack-objects failed: {}",
            errors.await.unwrap_or_default().trim()
        ))
    };
    wire::send(&mut send, &last).await?;
    send.finish()?;
    send.stopped().await.ok();
    Ok(())
}
