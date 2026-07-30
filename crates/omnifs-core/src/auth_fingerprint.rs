use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

/// Digest of the provider-declared runtime policy for one auth scheme.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AuthRuntimeFingerprint([u8; 32]);

impl AuthRuntimeFingerprint {
    #[must_use]
    pub const fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for AuthRuntimeFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for AuthRuntimeFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "AuthRuntimeFingerprint({self})")
    }
}

impl FromStr for AuthRuntimeFingerprint {
    type Err = AuthRuntimeFingerprintParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 64 {
            return Err(AuthRuntimeFingerprintParseError::BadLength { len: value.len() });
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(AuthRuntimeFingerprintParseError::NotLowerHex);
        }
        let mut bytes = [0_u8; 32];
        hex::decode_to_slice(value, &mut bytes)
            .map_err(|_| AuthRuntimeFingerprintParseError::NotLowerHex)?;
        Ok(Self(bytes))
    }
}

impl Serialize for AuthRuntimeFingerprint {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for AuthRuntimeFingerprint {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AuthRuntimeFingerprintParseError {
    #[error("auth runtime fingerprint must be 64 lowercase hex characters, got {len}")]
    BadLength { len: usize },
    #[error("auth runtime fingerprint must contain only lowercase hex characters")]
    NotLowerHex,
}
