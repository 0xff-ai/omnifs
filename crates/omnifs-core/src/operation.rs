use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

/// Random identity of one durable control-plane action.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ActionId([u8; 16]);

impl ActionId {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Display for ActionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for ActionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "ActionId({self})")
    }
}

impl FromStr for ActionId {
    type Err = ActionIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 32 {
            return Err(ActionIdError::BadLength { len: value.len() });
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ActionIdError::NotLowerHex);
        }
        let mut bytes = [0_u8; 16];
        hex::decode_to_slice(value, &mut bytes).map_err(|_| ActionIdError::NotLowerHex)?;
        Ok(Self(bytes))
    }
}

impl Serialize for ActionId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ActionId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ActionIdError {
    #[error("action id must be 32 lowercase hex characters, got {len}")]
    BadLength { len: usize },
    #[error("action id must contain only lowercase hex characters")]
    NotLowerHex,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_id_round_trips_through_display_and_parse() {
        let id = ActionId::from_bytes([0xab; 16]);
        assert_eq!(id.to_string(), "ab".repeat(16));
        assert_eq!(id.to_string().parse::<ActionId>().unwrap(), id);
    }
}
