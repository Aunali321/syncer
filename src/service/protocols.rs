//! What the service answers on its two protocols: members' requests and
//! invite redemptions.

use crate::{
    ids::{DeviceId, short},
    net::{
        proto::{Chunk, GetReply, Hello, PairReply, PairRequest, PullMessage, Request},
        wire,
    },
    sync::serve,
    util::lock,
};
use anyhow::{Result, ensure};
use iroh::{
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler},
};
use std::sync::{Arc, Weak};

use super::*;

/// Serves members on `ALPN`.
#[derive(Debug, Clone)]
pub(super) struct Members(pub(super) Weak<Service>);

impl ProtocolHandler for Members {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let peer = connection.remote_id();
        let Some(service) = self.0.upgrade() else {
            return Ok(());
        };

        if !lock(&service.cluster).is_member(&peer) {
            tracing::debug!("refusing {}: not a member", short(&peer));
            connection.close(2u32.into(), b"not a member");
            return Ok(());
        }

        tracing::debug!("{} connected", short(&peer));
        service.node.saw(&peer);
        let mut changes = service.cluster_changes.subscribe();
        drop(service);

        loop {
            let (send, recv) = tokio::select! {
                streams = connection.accept_bi() => match streams {
                    Ok(streams) => streams,
                    Err(_) => return Ok(()),
                },
                changed = changes.changed() => {
                    // A device removed from the cluster loses everything it
                    // has open here at once, pulls included.
                    let removed = changed.is_err()
                        || self.0.upgrade().is_none_or(|service| !lock(&service.cluster).is_member(&peer));
                    if removed {
                        connection.close(3u32.into(), b"removed");
                        return Ok(());
                    }
                    continue;
                }
            };
            let service = self.0.clone();
            tokio::spawn(async move {
                let Some(service) = service.upgrade() else {
                    return;
                };
                if let Err(e) = serve_stream(service, peer, send, recv).await {
                    tracing::debug!("serving {}: {e:#}", short(&peer));
                }
            });
        }
    }
}

async fn serve_stream(
    service: Arc<Service>,
    peer: DeviceId,
    mut send: iroh::endpoint::SendStream,
    mut recv: iroh::endpoint::RecvStream,
) -> Result<()> {
    let request: Request = wire::expect(&mut recv).await?;
    tracing::trace!("{} asks: {request:?}", short(&peer));

    // Every request but the cluster's is about a folder the peer holds too.
    let folder = match &request {
        Request::Cluster(_) => None,
        Request::Pull { folder, .. }
        | Request::Get { folder, .. }
        | Request::Pack { folder, .. } => {
            let running = match service.folder(folder) {
                Some(running) if service.peer_holds(&peer, folder).await => Some(running),
                _ => None,
            };
            match running {
                Some(running) => Some(running),
                None => {
                    let why = format!("{folder} is not shared with this device here");
                    match &request {
                        Request::Pull { .. } => {
                            wire::send(&mut send, &PullMessage::Refused(why)).await?
                        }
                        Request::Get { .. } => wire::send(&mut send, &GetReply::Missing).await?,
                        _ => wire::send(&mut send, &Chunk::Failed(why)).await?,
                    }
                    send.finish()?;
                    return Ok(());
                }
            }
        }
    };

    match (request, folder) {
        (Request::Cluster(hello), _) => {
            service.merge_cluster(&peer, &hello)?;
            wire::send(&mut send, &service.hello()).await?;
            send.finish()?;
            Ok(())
        }
        (Request::Pull { cursor, .. }, Some(folder)) => {
            drop(service);
            serve::pull(folder, send, cursor).await
        }
        (
            Request::Get {
                hash, path, offset, ..
            },
            Some(folder),
        ) => serve::get(folder, send, hash, path, offset).await,
        (Request::Pack { wants, haves, .. }, Some(folder)) => {
            serve::pack(folder, send, wants, haves).await
        }
        _ => Ok(()),
    }
}

/// Redeems invites on `PAIR_ALPN`.
#[derive(Debug, Clone)]
pub(super) struct Pairing(pub(super) Weak<Service>);

impl ProtocolHandler for Pairing {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let joiner = connection.remote_id();
        let Some(service) = self.0.upgrade() else {
            return Ok(());
        };
        let Ok((mut send, mut recv)) = connection.accept_bi().await else {
            return Ok(());
        };

        let reply = match pair(&service, joiner, &mut recv).await {
            Ok(hello) => PairReply::Welcome(hello),
            Err(e) => {
                tracing::info!("refused a join from {}: {e:#}", short(&joiner));
                PairReply::Refused(crate::util::describe(&e))
            }
        };
        if wire::send(&mut send, &reply).await.is_ok() {
            let _ = send.finish();
            let _ = send.stopped().await;
        }
        Ok(())
    }
}

async fn pair(
    service: &Service,
    joiner: DeviceId,
    recv: &mut iroh::endpoint::RecvStream,
) -> Result<Hello> {
    let request: PairRequest = wire::expect(recv).await?;
    service.invites.redeem(&request.secret)?;
    ensure!(
        request.addr.id == joiner,
        "the joining device named another address"
    );
    let name = request.name.trim();
    let name = if name.is_empty() {
        short(&joiner)
    } else {
        name.to_owned()
    };

    service.change_cluster(|cluster| cluster.set_device(joiner, &name, false))?;
    service.node.remember(request.addr);
    tracing::info!("{} ({}) joined", name, short(&joiner));
    Ok(service.hello())
}
