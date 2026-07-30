use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Content identity of a provider: the BLAKE3 digest of the exact provider
/// WASM bytes held by the host.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProviderId([u8; 32]);

impl ProviderId {
    #[must_use]
    pub const fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    #[must_use]
    pub fn from_wasm_bytes(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ProviderId({self})")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProviderIdHexError {
    #[error("provider id must be 64 hex characters, got {len}")]
    BadLength { len: usize },
    #[error("provider id must be lowercase hex (0-9a-f)")]
    NotHex,
}

impl FromStr for ProviderId {
    type Err = ProviderIdHexError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 64 {
            return Err(ProviderIdHexError::BadLength { len: value.len() });
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ProviderIdHexError::NotHex);
        }
        let mut bytes = [0u8; 32];
        hex::decode_to_slice(value, &mut bytes).map_err(|_| ProviderIdHexError::NotHex)?;
        Ok(Self(bytes))
    }
}

impl Serialize for ProviderId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ProviderId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::ProviderId;

    #[test]
    fn validates_and_round_trips_provider_identity() {
        let uppercase = "A".repeat(64);
        let bad_char = "g".repeat(64);
        for (label, hex) in [
            ("non-hex", "xyz"),
            ("uppercase", uppercase.as_str()),
            ("bad-char", bad_char.as_str()),
        ] {
            assert!(hex.parse::<ProviderId>().is_err(), "{label}");
        }

        let id = ProviderId::from_wasm_bytes(b"some wasm bytes");
        let display = id.to_string();
        assert_eq!(display.len(), 64);
        assert_eq!(display.parse::<ProviderId>().unwrap(), id);
        assert_ne!(ProviderId::from_wasm_bytes(b"other bytes"), id);

        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{id}\""));
        assert_eq!(serde_json::from_str::<ProviderId>(&json).unwrap(), id);
    }
}
