use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

/// Stable identity of one CLI-owned filesystem namespace.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClientOwnerId([u8; 16]);

impl ClientOwnerId {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Display for ClientOwnerId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for ClientOwnerId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "ClientOwnerId({self})")
    }
}

impl FromStr for ClientOwnerId {
    type Err = ClientOwnerIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 32 {
            return Err(ClientOwnerIdError::BadLength { len: value.len() });
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ClientOwnerIdError::NotLowerHex);
        }
        let mut bytes = [0_u8; 16];
        hex::decode_to_slice(value, &mut bytes).map_err(|_| ClientOwnerIdError::NotLowerHex)?;
        Ok(Self(bytes))
    }
}

impl Serialize for ClientOwnerId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ClientOwnerId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ClientOwnerIdError {
    #[error("client owner id must be 32 lowercase hex characters, got {len}")]
    BadLength { len: usize },
    #[error("client owner id must contain only lowercase hex characters")]
    NotLowerHex,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_and_round_trips() {
        let id = ClientOwnerId::from_bytes([0xab; 16]);
        assert_eq!(id.to_string(), "ab".repeat(16));
        assert_eq!(id.to_string().parse::<ClientOwnerId>().unwrap(), id);
        assert!("AB".repeat(16).parse::<ClientOwnerId>().is_err());
        assert!("ab".parse::<ClientOwnerId>().is_err());
    }
}
