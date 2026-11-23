//! The messages devices exchange. See DESIGN.md, "Wire protocol".
//!
//! Every bi-stream on `ALPN` starts with one `Request` from the side that
//! opened it; what follows depends on the request.

use crate::{
    cluster::ClusterState,
    folder::{
        index::{Entry, Hash, Replica},
        state::Cursor,
    },
    git::{GitState, Oid},
    ids::FolderId,
};
use iroh::EndpointAddr;
use serde::{Deserialize, Serialize};

/// Between members of a cluster.
pub const ALPN: &[u8] = b"syncer/1";
/// Redeeming an invite.
pub const PAIR_ALPN: &[u8] = b"syncer/pair/1";

/// The size of one chunk of file content or Git pack.
pub const CHUNK: usize = 256 * 1024;

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    /// Exchange cluster state: answered with `Hello`.
    Cluster(Hello),
    /// Stream the folder's index after `cursor`, then changes as they come:
    /// answered with `PullMessage`s.
    Pull { folder: FolderId, cursor: Cursor },
    /// Send the content with `hash`, preferably from `path`, from `offset`:
    /// answered with `GetReply` and then `Chunk`s.
    Get {
        folder: FolderId,
        hash: Hash,
        path: String,
        offset: u64,
    },
    /// Send a pack of `wants` without what `haves` reach: answered with
    /// `Chunk`s.
    Pack {
        folder: FolderId,
        wants: Vec<Oid>,
        haves: Vec<Oid>,
    },
}

/// A device's cluster state and how to reach it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub state: ClusterState,
    pub addr: EndpointAddr,
    /// Where the sender last reached other members, for devices that cannot
    /// look addresses up (no relays, no local discovery).
    pub known: Vec<EndpointAddr>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum PullMessage {
    /// First: which replica the index is, so a cursor for another starts over.
    Start {
        replica: Replica,
    },
    /// The sender's Git state, sent first and whenever it changes.
    Git(GitState),
    /// Changes up to the sender's sequence number `upto`, made while its
    /// checkout was at `head`. An empty batch says the puller is caught up.
    Batch {
        entries: Vec<Entry>,
        upto: u64,
        head: Option<Oid>,
    },
    Refused(String),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum GetReply {
    /// The content follows: `size` bytes in all, from the asked offset on.
    Found {
        size: u64,
    },
    Missing,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Chunk {
    Data(#[serde(with = "serde_bytes")] Vec<u8>),
    End,
    Failed(String),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PairRequest {
    #[serde(with = "serde_bytes")]
    pub secret: [u8; 32],
    pub name: String,
    pub addr: EndpointAddr,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum PairReply {
    Welcome(Hello),
    Refused(String),
}
