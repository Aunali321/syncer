//! One-use invites. See DESIGN.md, "Pairing".

use crate::util::lock;
use anyhow::{Context, Result, ensure};
use iroh::EndpointAddr;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

/// How long an invite can be redeemed.
pub const INVITE_TTL: Duration = Duration::from_secs(15 * 60);
const PREFIX: &str = "syncer1";

/// What a device needs to join: where the inviter is, and the secret.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ticket {
    pub addr: EndpointAddr,
    #[serde(with = "serde_bytes")]
    pub secret: [u8; 32],
}

impl Ticket {
    pub fn encode(&self) -> String {
        let bytes = rmp_serde::to_vec(self).expect("a ticket always encodes");
        format!("{PREFIX}{}", data_encoding::BASE32_NOPAD.encode(&bytes).to_ascii_lowercase())
    }

    pub fn decode(text: &str) -> Result<Self> {
        let body = text.trim().strip_prefix(PREFIX).context("not a syncer invite")?;
        let bytes = data_encoding::BASE32_NOPAD
            .decode(body.to_ascii_uppercase().as_bytes())
            .context("the invite is damaged")?;
        rmp_serde::from_slice(&bytes).context("the invite is damaged")
    }
}

/// The invites this device has given out and not yet seen redeemed.
#[derive(Default)]
pub struct Invites {
    open: Mutex<HashMap<[u8; 32], Instant>>,
}

impl Invites {
    pub fn create(&self) -> [u8; 32] {
        let secret: [u8; 32] = rand::random();
        let mut open = lock(&self.open);
        open.retain(|_, expires| *expires > Instant::now());
        open.insert(secret, Instant::now() + INVITE_TTL);
        secret
    }

    /// Uses up the invite with `secret`, if it is open.
    pub fn redeem(&self, secret: &[u8; 32]) -> Result<()> {
        let expires = lock(&self.open).remove(secret).context("unknown or used invite")?;
        ensure!(expires > Instant::now(), "the invite has expired");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tickets_round_trip_and_invites_are_used_once() {
        let invites = Invites::default();
        let secret = invites.create();
        let ticket = Ticket {
            addr: EndpointAddr::new(iroh::SecretKey::from_bytes(&[3; 32]).public()),
            secret,
        };

        let decoded = Ticket::decode(&ticket.encode()).expect("decodes");
        assert_eq!(decoded.addr, ticket.addr);
        assert!(invites.redeem(&decoded.secret).is_ok());
        assert!(invites.redeem(&decoded.secret).is_err());
        assert!(Ticket::decode("syncer1notbase32!").is_err());
    }
}
