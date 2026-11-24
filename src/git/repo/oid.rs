//! Git object ids, and what HEAD points at.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::fmt;

/// A Git object id, as lowercase hex (SHA-1 or SHA-256).
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Oid(String);

impl Oid {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Oid {
    type Error = anyhow::Error;

    fn try_from(text: String) -> Result<Self> {
        ensure!(
            matches!(text.len(), 40 | 64)
                && text
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "not a Git object id: {text:?}"
        );
        Ok(Self(text))
    }
}

impl From<Oid> for String {
    fn from(oid: Oid) -> Self {
        oid.0
    }
}

impl fmt::Debug for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0[..12])
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What HEAD points at.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HeadTarget {
    /// A branch by short name, which may not exist yet (an unborn branch).
    Branch(String),
    Detached(Oid),
}
