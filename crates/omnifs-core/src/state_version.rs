use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::num::NonZeroU64;
use std::str::FromStr;

macro_rules! nonzero_version {
    ($name:ident) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(NonZeroU64);

        impl $name {
            #[must_use]
            pub const fn new(value: NonZeroU64) -> Self {
                Self(value)
            }

            #[must_use]
            pub const fn initial() -> Self {
                Self(NonZeroU64::MIN)
            }

            #[must_use]
            pub const fn get(self) -> u64 {
                self.0.get()
            }

            pub const fn next(self) -> Option<Self> {
                match self.get().checked_add(1) {
                    Some(value) => match NonZeroU64::new(value) {
                        Some(value) => Some(Self(value)),
                        None => None,
                    },
                    None => None,
                }
            }
        }
    };
}

nonzero_version!(CredentialVersion);
nonzero_version!(CredentialGeneration);

/// Durable compare-and-swap version of one mount document.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MountVersion([u8; 32]);

impl MountVersion {
    #[must_use]
    pub const fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for MountVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for MountVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "MountVersion({self})")
    }
}

impl FromStr for MountVersion {
    type Err = MountVersionParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 64 {
            return Err(MountVersionParseError::BadLength { len: value.len() });
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(MountVersionParseError::NotLowerHex);
        }
        let mut bytes = [0_u8; 32];
        hex::decode_to_slice(value, &mut bytes).map_err(|_| MountVersionParseError::NotLowerHex)?;
        Ok(Self(bytes))
    }
}

impl Serialize for MountVersion {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for MountVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MountVersionParseError {
    #[error("mount version must be 64 lowercase hex characters, got {len}")]
    BadLength { len: usize },
    #[error("mount version must contain only lowercase hex characters")]
    NotLowerHex,
}
