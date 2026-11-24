//! This device on the network: an iroh endpoint, one reused connection per
//! peer, and the addresses peers were last reached at.

use crate::{
    ids::DeviceId,
    net::proto::ALPN,
    util::{lock, write_atomic},
};
use anyhow::{Context, Result};
use iroh::{
    Endpoint, EndpointAddr, RelayMode, RelayUrl, SecretKey,
    endpoint::{Connection, IdleTimeout, QuicTransportConfig, VarInt, presets},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime},
};
use tokio::time::timeout;

/// How often an idle connection is pinged, so NAT mappings stay open and a
/// dead peer is noticed.
const KEEP_ALIVE: Duration = Duration::from_secs(10);
/// How long a connection may hear nothing before it counts as dead.
const IDLE_MS: u32 = 45_000;
/// How long a connection may take to open.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// Which relays carry traffic between devices that cannot reach each other
/// directly.
#[derive(Debug, Clone, Default)]
pub enum Relays {
    /// iroh's public relays, with its DNS address lookup.
    #[default]
    Public,
    /// These relays (run with `iroh-relay`), with iroh's DNS address lookup.
    Own(Vec<RelayUrl>),
    /// None, and no DNS lookup: only direct addresses and the local network.
    Off,
}

/// How the endpoint finds and reaches peers.
#[derive(Debug, Clone)]
pub struct NetOptions {
    /// UDP port to bind; 0 reuses the port of the last run, so peers that
    /// remember this device's address still reach it, or picks one.
    pub port: u16,
    pub relays: Relays,
    /// Finding peers on the local network.
    pub mdns: bool,
}

impl Default for NetOptions {
    fn default() -> Self {
        Self {
            port: 0,
            relays: Relays::Public,
            mdns: true,
        }
    }
}

/// How an open connection reaches a peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Link {
    Direct,
    /// Through a relay: slower, and public relays limit how much they carry.
    Relayed,
}

/// Binds the endpoint. With no port asked for, the port of the last run
/// (kept in `port_file`) is tried first, then any.
pub async fn bind(secret: SecretKey, options: &NetOptions, port_file: &Path) -> Result<Endpoint> {
    let remembered = std::fs::read_to_string(port_file)
        .ok()
        .and_then(|text| text.trim().parse::<u16>().ok());
    let port = if options.port != 0 {
        Some(options.port)
    } else {
        remembered
    };

    let endpoint = match port {
        Some(port) => match bind_on(secret.clone(), options, port).await {
            Ok(endpoint) => endpoint,
            Err(e) if options.port == 0 => {
                tracing::warn!("UDP port {port} is taken, using another: {e:#}");
                bind_on(secret, options, 0).await?
            }
            Err(e) => return Err(e),
        },
        None => bind_on(secret, options, 0).await?,
    };

    if let Some(bound) = endpoint.bound_sockets().first().map(|addr| addr.port())
        && remembered != Some(bound)
        && let Err(e) = write_atomic(port_file, bound.to_string().as_bytes())
    {
        tracing::warn!("remembering the UDP port: {e:#}");
    }

    Ok(endpoint)
}

async fn bind_on(secret: SecretKey, options: &NetOptions, port: u16) -> Result<Endpoint> {
    // Every datagram on its own: some network cards silently drop batched
    // ones, which stalls large transfers on connections that look healthy.
    let transport = QuicTransportConfig::builder()
        .enable_segmentation_offload(false)
        .keep_alive_interval(KEEP_ALIVE)
        .max_idle_timeout(Some(IdleTimeout::from(VarInt::from_u32(IDLE_MS))))
        .build();
    let mut builder = match &options.relays {
        Relays::Public => Endpoint::builder(presets::N0),
        Relays::Own(urls) => {
            Endpoint::builder(presets::N0).relay_mode(RelayMode::custom(urls.iter().cloned()))
        }
        Relays::Off => Endpoint::builder(presets::Minimal),
    }
    .secret_key(secret)
    .transport_config(transport);

    if options.mdns {
        builder = builder.address_lookup(
            iroh_mdns_address_lookup::MdnsAddressLookup::builder().service_name("syncer"),
        );
    }

    if port != 0 {
        builder = builder
            .clear_ip_transports()
            .bind_addr(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)))?
            .bind_addr_with_opts(
                SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)),
                iroh::endpoint::BindOpts::default().set_is_required(false),
            )?;
    }

    builder.bind().await.context("binding the network endpoint")
}

pub struct Node {
    endpoint: Endpoint,
    /// Connections this device opened. Each side opens its own: streams are
    /// only accepted on connections the other side opened.
    connections: Mutex<HashMap<DeviceId, Connection>>,
    addrs: Mutex<HashMap<DeviceId, EndpointAddr>>,
    addrs_file: PathBuf,
    /// When each peer was last heard from, kept across restarts.
    seen: Mutex<BTreeMap<DeviceId, SystemTime>>,
    seen_file: PathBuf,
}

impl Node {
    pub fn new(endpoint: Endpoint, addrs_file: PathBuf, seen_file: PathBuf) -> Self {
        let addrs = std::fs::read(&addrs_file)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Vec<EndpointAddr>>(&bytes).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|addr| (addr.id, addr))
            .collect();

        let seen = std::fs::read(&seen_file)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();

        Self {
            endpoint,
            connections: Mutex::default(),
            addrs: Mutex::new(addrs),
            addrs_file,
            seen: Mutex::new(seen),
            seen_file,
        }
    }

    /// Notes that `peer` was heard from just now.
    pub fn saw(&self, peer: &DeviceId) {
        let seen = {
            let mut seen = lock(&self.seen);
            seen.insert(*peer, SystemTime::now());
            seen.clone()
        };
        if let Err(e) = serde_json::to_vec_pretty(&seen)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| write_atomic(&self.seen_file, &bytes))
        {
            tracing::warn!("saving when peers were seen: {e:#}");
        }
    }

    /// When `peer` was last heard from, if ever.
    pub fn last_seen(&self, peer: &DeviceId) -> Option<SystemTime> {
        lock(&self.seen).get(peer).copied()
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

        if let Err(e) = serde_json::to_vec_pretty(&all)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| write_atomic(&self.addrs_file, &bytes))
        {
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

        let addr = lock(&self.addrs)
            .get(&peer)
            .cloned()
            .unwrap_or_else(|| EndpointAddr::new(peer));
        let connection = timeout(CONNECT_TIMEOUT, self.endpoint.connect(addr, ALPN))
            .await
            .context("timed out connecting")?
            .context("connecting")?;
        tracing::debug!("connected to {}", crate::ids::short(&peer));
        lock(&self.connections).insert(peer, connection.clone());
        self.saw(&peer);
        Ok(connection)
    }

    /// Closes the connection to a peer, as when it is removed.
    pub fn drop_peer(&self, peer: &DeviceId) {
        if let Some(connection) = lock(&self.connections).remove(peer) {
            connection.close(VarInt::from_u32(1), b"removed");
        }
    }

    pub fn is_connected(&self, peer: &DeviceId) -> bool {
        self.link(peer).is_some()
    }

    /// How the open connection to `peer` reaches it, if one is open.
    pub fn link(&self, peer: &DeviceId) -> Option<Link> {
        let connections = lock(&self.connections);
        let connection = connections
            .get(peer)
            .filter(|connection| connection.close_reason().is_none())?;
        let relayed = connection
            .paths()
            .iter()
            .find(|path| path.is_selected())
            .is_some_and(|path| path.is_relay());
        Some(if relayed { Link::Relayed } else { Link::Direct })
    }
}
