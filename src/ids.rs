//! Names for devices and folders.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::fmt;

/// A device, by its iroh public key.
pub type DeviceId = iroh::EndpointId;

/// A shared folder, the same on every device that holds it.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct FolderId(String);

const FOLDER_ID_LEN: usize = 12;
const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";

impl FolderId {
    pub fn random() -> Self {
        let id = (0..FOLDER_ID_LEN)
            .map(|_| char::from(ALPHABET[rand::random_range(0..ALPHABET.len())]))
            .collect();
        Self(id)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for FolderId {
    type Error = anyhow::Error;

    fn try_from(text: String) -> Result<Self> {
        ensure!(
            text.len() == FOLDER_ID_LEN && text.bytes().all(|b| ALPHABET.contains(&b)),
            "not a folder id: {text:?}"
        );
        Ok(Self(text))
    }
}

impl std::str::FromStr for FolderId {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        Self::try_from(text.to_owned())
    }
}

impl From<FolderId> for String {
    fn from(id: FolderId) -> Self {
        id.0
    }
}

impl fmt::Display for FolderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for FolderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A device id as people read it: its first ten characters.
pub fn short(device: &DeviceId) -> String {
    device.fmt_short().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_ids_round_trip() {
        let id = FolderId::random();
        assert_eq!(id.as_str().parse::<FolderId>().expect("parses"), id);
        assert!("UPPERCASE123".parse::<FolderId>().is_err());
        assert!("short".parse::<FolderId>().is_err());
    }
}
