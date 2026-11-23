//! This device on the network: an iroh endpoint, one reused connection per
//! peer, and the addresses peers were last reached at.

use crate::{
    ids::DeviceId,
    net::proto::ALPN,
    util::{lock, write_atomic},
};
use anyhow::{Context, Result};
use iroh::{
    Endpoint, EndpointAddr, SecretKey,
    endpoint::{Connection, IdleTimeout, QuicTransportConfig, VarInt, presets},
};
use std::{
    collections::HashMap,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
    sync::Mutex,
    time::Duration,
};
use tokio::time::timeout;

/// How often an idle connection is pinged, so NAT mappings stay open and a
/// dead peer is noticed.
const KEEP_ALIVE: Duration = Duration::from_secs(10);
/// How long a connection may hear nothing before it counts as dead.
const IDLE_MS: u32 = 45_000;
/// How long a connection may take to open.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// How the endpoint finds and reaches peers.
#[derive(Debug, Clone)]
pub struct NetOptions {
    /// UDP port to bind; 0 picks one.
    pub port: u16,
    /// Public relays and DNS address lookup (iroh's n0 services). Off, only
    /// direct addresses and the local network work.
    pub relays: bool,
    /// Finding peers on the local network.
    pub mdns: bool,
}

impl Default for NetOptions {
    fn default() -> Self {
        Self {
            port: 0,
            relays: true,
            mdns: true,
        }
    }
}

pub async fn bind(secret: SecretKey, options: &NetOptions) -> Result<Endpoint> {
    // Every datagram on its own: some network cards silently drop batched
    // ones, which stalls large transfers on connections that look healthy.
    let transport = QuicTransportConfig::builder()
        .enable_segmentation_offload(false)
        .keep_alive_interval(KEEP_ALIVE)
        .max_idle_timeout(Some(IdleTimeout::from(VarInt::from_u32(IDLE_MS))))
        .build();
    let mut builder = if options.relays {
        Endpoint::builder(presets::N0)
    } else {
        Endpoint::builder(presets::Minimal)
    }
    .secret_key(secret)
    .transport_config(transport);

    if options.mdns {
        builder = builder.address_lookup(
            iroh_mdns_address_lookup::MdnsAddressLookup::builder().service_name("syncer"),
        );
    }

    if options.port != 0 {
        builder = builder
            .clear_ip_transports()
            .bind_addr(SocketAddr::from((Ipv4Addr::UNSPECIFIED, options.port)))?
            .bind_addr_with_opts(
                SocketAddr::from((Ipv6Addr::UNSPECIFIED, options.port)),
                iroh::endpoint::BindOpts::default().set_is_required(false),
            )?;
    }

    builder.bind().await.context("binding the network endpoint")
}

pub struct Node {
    endpoint: Endpoint,
    connections: Mutex<HashMap<DeviceId, Connection>>,
    addrs: Mutex<HashMap<DeviceId, EndpointAddr>>,
    addrs_file: PathBuf,
}

impl Node {
    pub fn new(endpoint: Endpoint, addrs_file: PathBuf) -> Self {
        let addrs = std::fs::read(&addrs_file)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Vec<EndpointAddr>>(&bytes).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|addr| (addr.id, addr))
            .collect();

        Self {
            endpoint,
            connections: Mutex::default(),
            addrs: Mutex::new(addrs),
            addrs_file,
        }
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    pub fn id(&self) -> DeviceId {
        self.endpoint.id()
    }

    pub fn addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }

    /// Remembers where `addr`'s device was reachable, for the next start.
    pub fn remember(&self, addr: EndpointAddr) {
        if addr.id == self.id() || addr.is_empty() {
            return;
        }

        let all: Vec<EndpointAddr> = {
            let mut addrs = lock(&self.addrs);
            if addrs.get(&addr.id) == Some(&addr) {
                return;
            }
            addrs.insert(addr.id, addr);
            addrs.values().cloned().collect()
        };

        if let Err(e) = serde_json::to_vec_pretty(&all).map_err(anyhow::Error::from).and_then(|bytes| write_atomic(&self.addrs_file, &bytes)) {
            tracing::warn!("saving peer addresses: {e:#}");
        }
    }

    /// Remembers `addr` only when nothing is known about its device yet.
    pub fn learn(&self, addr: EndpointAddr) {
        if !lock(&self.addrs).contains_key(&addr.id) {
            self.remember(addr);
        }
    }

    /// The addresses peers were last reached at.
    pub fn known(&self) -> Vec<EndpointAddr> {
        lock(&self.addrs).values().cloned().collect()
    }

    /// A connection to `peer`, reusing the open one.
    pub async fn connect(&self, peer: DeviceId) -> Result<Connection> {
        if let Some(connection) = lock(&self.connections).get(&peer)
            && connection.close_reason().is_none()
        {
            return Ok(connection.clone());
        }

        let addr = lock(&self.addrs).get(&peer).cloned().unwrap_or_else(|| EndpointAddr::new(peer));
        let connection = timeout(CONNECT_TIMEOUT, self.endpoint.connect(addr, ALPN))
            .await
            .context("timed out connecting")?
            .context("connecting")?;
        lock(&self.connections).insert(peer, connection.clone());
        Ok(connection)
    }

    /// Keeps a connection a peer opened, for streams this side opens.
    pub fn adopt(&self, connection: &Connection) {
        let peer = connection.remote_id();
        let mut connections = lock(&self.connections);
        let keep = connections.get(&peer).is_none_or(|open| open.close_reason().is_some());
        if keep {
            connections.insert(peer, connection.clone());
        }
    }

    /// Closes the connection to a peer, as when it is removed.
    pub fn drop_peer(&self, peer: &DeviceId) {
        if let Some(connection) = lock(&self.connections).remove(peer) {
            connection.close(VarInt::from_u32(1), b"removed");
        }
    }

    pub fn is_connected(&self, peer: &DeviceId) -> bool {
        lock(&self.connections)
            .get(peer)
            .is_some_and(|connection| connection.close_reason().is_none())
    }
}
