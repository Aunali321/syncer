//! The cluster: exchanging its state with members, and joining one.

use crate::{
    ids::{DeviceId, FolderId, short},
    net::{
        node::{self},
        pair::Ticket,
        proto::{Hello, PAIR_ALPN, PairReply, PairRequest, Request},
        wire,
    },
    util::lock,
};
use anyhow::{Context, Result, bail, ensure};
use std::{sync::Weak, time::Duration};
use tokio::time::timeout;

use super::*;

/// How often members exchange cluster state when nothing changed.
const GOSSIP: Duration = Duration::from_secs(60);
/// The first pause after a failed exchange, doubling up to `GOSSIP`.
const GOSSIP_RETRY: Duration = Duration::from_secs(2);
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);

impl Service {
    /// Merges a member's cluster state and notes where it was reached.
    pub(super) fn merge_cluster(&self, from: &DeviceId, hello: &Hello) -> Result<()> {
        if hello.addr.id == *from {
            self.node.remember(hello.addr.clone());
        }

        let changed = lock(&self.cluster).merge(&hello.state);
        {
            let cluster = lock(&self.cluster);
            for addr in &hello.known {
                if cluster.is_member(&addr.id) {
                    self.node.learn(addr.clone());
                }
            }
        }
        if changed {
            self.save_cluster()?;
            self.cluster_changes
                .send_modify(|generation| *generation += 1);
            let removed: Vec<DeviceId> = {
                let cluster = lock(&self.cluster);
                cluster
                    .state()
                    .devices
                    .iter()
                    .filter(|(_, record)| record.value.removed)
                    .map(|(id, _)| *id)
                    .collect()
            };
            for device in removed {
                self.node.drop_peer(&device);
            }
        }
        Ok(())
    }

    pub(super) fn hello(&self) -> Hello {
        Hello {
            state: lock(&self.cluster).state().clone(),
            addr: self.node.addr(),
            known: self.node.known(),
        }
    }

    /// Exchanges cluster state with `peer`.
    async fn exchange(&self, peer: DeviceId) -> Result<()> {
        let connection = self.node.connect(peer).await?;
        let (mut send, mut recv) = connection.open_bi().await?;
        wire::send(&mut send, &Request::Cluster(self.hello())).await?;
        send.finish()?;
        let theirs: Hello = wire::expect(&mut recv).await?;
        self.node.saw(&peer);
        self.merge_cluster(&peer, &theirs)
    }

    /// Whether `peer` holds `folder`. When this device has not heard so, the
    /// peer is asked first: a device that just took a folder asks for it at
    /// once, before gossip has told everyone.
    pub(super) async fn peer_holds(&self, peer: &DeviceId, folder: &FolderId) -> bool {
        if lock(&self.cluster).holds(peer, folder) {
            return true;
        }
        match timeout(EXCHANGE_TIMEOUT, self.exchange(*peer)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::debug!("cluster exchange with {}: {e:#}", short(peer)),
            Err(_) => tracing::debug!("cluster exchange with {} timed out", short(peer)),
        }
        lock(&self.cluster).holds(peer, folder)
    }

    /// A one-use invite into this cluster.
    pub async fn invite(&self) -> Result<String> {
        // Give the endpoint a moment to learn its addresses after start.
        let _ = timeout(Duration::from_secs(5), self.node.endpoint().online()).await;
        let secret = self.invites.create();
        Ok(Ticket {
            addr: self.node.addr(),
            secret,
        }
        .encode())
    }

    /// Joins the cluster a ticket invites into. This device's own cluster
    /// comes along: every device it trusts becomes trusted by the inviter's
    /// devices too, so that needs `merge`.
    pub async fn join(&self, ticket: &str, merge: bool) -> Result<String> {
        let ticket = Ticket::decode(ticket)?;
        ensure!(
            ticket.addr.id != self.me(),
            "That invite is from this device."
        );
        // Already trusting the inviter means already in its cluster.
        let trusted: Vec<String> = {
            let cluster = lock(&self.cluster);
            if cluster.is_member(&ticket.addr.id) {
                Vec::new()
            } else {
                cluster
                    .others()
                    .iter()
                    .map(|device| cluster.name_of(device))
                    .collect()
            }
        };
        ensure!(
            merge || trusted.is_empty(),
            "Joining would also connect {}. Run again with --merge to allow it.",
            trusted.join(", ")
        );
        let connection = timeout(
            node::CONNECT_TIMEOUT,
            self.node.endpoint().connect(ticket.addr.clone(), PAIR_ALPN),
        )
        .await
        .context("timed out reaching the inviting device")?
        .context("reaching the inviting device")?;
        let (mut send, mut recv) = connection.open_bi().await?;
        let request = PairRequest {
            secret: ticket.secret,
            name: self.name(),
            addr: self.node.addr(),
        };
        wire::send(&mut send, &request).await?;
        send.finish()?;

        let hello = match wire::expect::<PairReply>(&mut recv).await? {
            PairReply::Welcome(hello) => hello,
            PairReply::Refused(why) => bail!("the invite was refused: {why}"),
        };
        connection.close(0u32.into(), b"joined");
        let inviter = ticket.addr.id;
        self.node.remember(hello.addr.clone());
        lock(&self.cluster).merge(&hello.state);
        for addr in hello.known {
            self.node.learn(addr);
        }
        self.save_cluster()?;
        self.cluster_changes
            .send_modify(|generation| *generation += 1);
        Ok(lock(&self.cluster).name_of(&inviter))
    }

    pub fn forget_device(&self, device: &DeviceId) -> Result<()> {
        ensure!(
            *device != self.me(),
            "this is this device; remove it from another one"
        );
        let name = lock(&self.cluster).name_of(device);
        self.change_cluster(|cluster| cluster.set_device(*device, &name, true))?;
        self.node.drop_peer(device);
        Ok(())
    }

    pub fn rename(&self, name: &str) -> Result<()> {
        ensure!(!name.trim().is_empty(), "a name cannot be empty");
        let me = self.me();
        self.change_cluster(|cluster| cluster.set_device(me, name.trim(), false))
    }
}

pub(super) async fn gossip(service: Weak<Service>) {
    let mut changes = match service.upgrade() {
        Some(service) => service.cluster_changes.subscribe(),
        None => return,
    };
    let mut backoff = GOSSIP_RETRY;

    loop {
        let Some(service) = service.upgrade() else {
            return;
        };
        // Marked seen before the state is read, so a change made while this
        // round runs starts another.
        changes.borrow_and_update();
        let peers = lock(&service.cluster).others();
        let mut exchanges = tokio::task::JoinSet::new();
        for peer in peers {
            let service = service.clone();
            exchanges.spawn(async move {
                match timeout(EXCHANGE_TIMEOUT, service.exchange(peer)).await {
                    Ok(Ok(())) => true,
                    Ok(Err(e)) => {
                        tracing::debug!("cluster exchange with {}: {e:#}", short(&peer));
                        false
                    }
                    Err(_) => {
                        tracing::debug!("cluster exchange with {} timed out", short(&peer));
                        false
                    }
                }
            });
        }
        drop(service);
        let mut failed = false;
        while let Some(done) = exchanges.join_next().await {
            failed |= !done.unwrap_or(false);
        }

        // A failed exchange (with a device mid-join that does not know this
        // one yet, say) is tried again soon, backing off to the usual pace.
        let pause = if failed {
            let pause = backoff;
            backoff = (backoff * 2).min(GOSSIP);
            pause
        } else {
            backoff = GOSSIP_RETRY;
            GOSSIP
        };
        let _ = timeout(pause, changes.changed()).await;
    }
}
