//! Following one peer's index of one folder: its Git state first, then its
//! file changes, held back while the two checkouts are at different commits.

use crate::{
    folder::{Folder, state::Cursor},
    git::Oid,
    ids::{DeviceId, short},
    net::{
        node::Node,
        proto::{PullMessage, Request},
        wire,
    },
    sync::fetch,
};
use anyhow::{Result, bail};
use std::{sync::Arc, time::Duration};
use tokio::time::sleep;

/// The longest wait between attempts to reach a peer.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// How often a held-back pull tries Git alignment again.
const RETRY_HELD: Duration = Duration::from_secs(15);

/// Follows `peer`'s index of `folder` until the task is stopped.
pub async fn follow(node: Arc<Node>, folder: Arc<Folder>, peer: DeviceId) {
    let mut backoff = Duration::from_secs(1);

    loop {
        match once(&node, &folder, peer).await {
            // The checkouts met again: pull from the cursor at once.
            Ok(()) => backoff = Duration::from_secs(1),
            Err(e) => {
                tracing::debug!("{}: pulling from {}: {e:#}", folder.id, short(&peer));
                folder.set_waiting(&peer, Some(format!("not reachable: {e:#}")));
                sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

/// Pulls until the stream fails, or until changes that were held back can
/// be taken (`Ok`), which needs a fresh pull from the cursor.
async fn once(node: &Node, folder: &Arc<Folder>, peer: DeviceId) -> Result<()> {
    let connection = node.connect(peer).await?;
    let (mut send, mut recv) = connection.open_bi().await?;
    let mut cursor = folder.cursor(&peer);
    wire::send(
        &mut send,
        &Request::Pull {
            folder: folder.id.clone(),
            cursor,
        },
    )
    .await?;

    let mut local_git = folder.git_state();
    local_git.borrow_and_update();
    // While held, batches are skipped and the cursor stays where it was.
    let mut held = false;
    let mut peer_head: Option<Oid> = None;
    // The peer's last Git state, aligned with again while held: an
    // alignment skipped for a busy repository or a failed fetch is retried.
    let mut peer_git = None;
    let mut retry = tokio::time::interval(RETRY_HELD);
    retry.tick().await;

    loop {
        let message = tokio::select! {
            message = wire::recv::<PullMessage>(&mut recv) => message?,
            changed = local_git.changed() => {
                changed?;
                local_git.borrow_and_update();
                // The repository here moved away from the peer's (or went
                // away entirely): settle the peer's state into it again.
                if !folder.accepts(peer_head.as_ref())
                    && let Some(state) = &peer_git
                    && let Err(e) = folder
                        .align_git(&peer, state, |wants, haves| fetch::pack(&connection, folder, wants, haves))
                        .await
                {
                    tracing::debug!("{}: Git from {}: {e:#}", folder.id, short(&peer));
                }
                if held && folder.accepts(peer_head.as_ref()) {
                    return Ok(());
                }
                continue;
            }
            _ = retry.tick(), if held => {
                if let Some(state) = &peer_git
                    && let Err(e) = folder
                        .align_git(&peer, state, |wants, haves| fetch::pack(&connection, folder, wants, haves))
                        .await
                {
                    tracing::debug!("{}: Git from {}: {e:#}", folder.id, short(&peer));
                }
                if folder.accepts(peer_head.as_ref()) {
                    return Ok(());
                }
                continue;
            }
        };
        let Some(message) = message else {
            bail!("the peer closed the stream");
        };

        match message {
            PullMessage::Start { replica } => {
                if replica != cursor.replica {
                    cursor = Cursor { replica, seq: 0 };
                }
            }
            PullMessage::Git(state) => {
                peer_head = state.head_commit.clone();
                peer_git = Some(state.clone());
                let aligned = folder
                    .align_git(&peer, &state, |wants, haves| {
                        fetch::pack(&connection, folder, wants, haves)
                    })
                    .await;
                if let Err(e) = aligned {
                    tracing::warn!("{}: Git from {}: {e:#}", folder.id, short(&peer));
                    folder.set_waiting(
                        &peer,
                        Some(format!("Git sync failed: {}", crate::util::describe(&e))),
                    );
                    held = true;
                    continue;
                }
                if held && folder.accepts(peer_head.as_ref()) {
                    return Ok(());
                }
            }
            PullMessage::Batch {
                entries,
                upto,
                head,
            } => {
                peer_head = head;
                if held {
                    continue;
                }
                if !entries.is_empty() && !folder.accepts(peer_head.as_ref()) {
                    let why = format!(
                        "different commit checked out ({} there, {} here)",
                        describe(peer_head.as_ref()),
                        describe(folder.head_commit().as_ref())
                    );
                    folder.set_waiting(&peer, Some(why));
                    held = true;
                    continue;
                }

                cursor.seq = upto;
                let caught_up = entries.is_empty();
                folder.merge(&peer, &entries, cursor);
                folder.set_waiting(&peer, None);
                // The peer ends what it has with an empty batch.
                if caught_up {
                    folder.caught_up_with(&peer);
                }
            }
            PullMessage::Refused(why) => bail!("refused: {why}"),
        }
    }
}

fn describe(commit: Option<&Oid>) -> String {
    commit.map_or_else(
        || "no commit".to_owned(),
        |oid| oid.as_str()[..12].to_owned(),
    )
}
