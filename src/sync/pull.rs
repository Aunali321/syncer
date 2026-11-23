//! Following one peer's index of one folder: its Git state first, then its
//! file changes, held back while the two checkouts are at different commits.

use crate::{
    folder::{Folder, state::Cursor},
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
use tokio::time::{sleep, timeout};

/// The longest wait between attempts to reach a peer.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// How long a puller held back by Git waits before asking again, unless the
/// checkout here moves first.
const RECHECK: Duration = Duration::from_secs(30);

/// How a pull ended.
enum Ended {
    /// The peer's checkout is elsewhere; wait for Git to catch up.
    HeldBack,
}

/// Follows `peer`'s index of `folder` until the task is stopped.
pub async fn follow(node: Arc<Node>, folder: Arc<Folder>, peer: DeviceId) {
    let mut backoff = Duration::from_secs(1);

    loop {
        match once(&node, &folder, peer).await {
            Ok(Ended::HeldBack) => {
                backoff = Duration::from_secs(1);
                let mut git = folder.git_state();
                git.borrow_and_update();
                let _ = timeout(RECHECK, git.changed()).await;
            }
            Err(e) => {
                tracing::debug!("{}: pulling from {}: {e:#}", folder.id, short(&peer));
                folder.set_waiting(&peer, Some(format!("unreachable: {e:#}")));
                sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

async fn once(node: &Node, folder: &Arc<Folder>, peer: DeviceId) -> Result<Ended> {
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

    loop {
        let Some(message) = wire::recv::<PullMessage>(&mut recv).await? else {
            bail!("the peer closed the stream");
        };

        match message {
            PullMessage::Start { replica } => {
                if replica != cursor.replica {
                    cursor = Cursor { replica, seq: 0 };
                }
            }
            PullMessage::Git(state) => {
                let aligned = folder
                    .align_git(&peer, &state, |wants, haves| fetch::pack(&connection, folder, wants, haves))
                    .await;
                if let Err(e) = aligned {
                    tracing::warn!("{}: Git from {}: {e:#}", folder.id, short(&peer));
                    folder.set_waiting(&peer, Some(format!("Git: {e:#}")));
                    return Ok(Ended::HeldBack);
                }
            }
            PullMessage::Batch { entries, upto, head } => {
                if !entries.is_empty() && !folder.accepts(head.as_ref()) {
                    let why = format!(
                        "its checkout is at {}, this one at {}",
                        describe(head.as_ref()),
                        describe(folder.head_commit().as_ref())
                    );
                    folder.set_waiting(&peer, Some(why));
                    return Ok(Ended::HeldBack);
                }

                cursor.seq = upto;
                folder.merge(&peer, &entries, cursor);
                folder.set_waiting(&peer, None);
            }
            PullMessage::Refused(why) => bail!("refused: {why}"),
        }
    }
}

fn describe(commit: Option<&crate::git::Oid>) -> String {
    commit.map_or_else(|| "no commit".to_owned(), |oid| oid.as_str()[..12].to_owned())
}
