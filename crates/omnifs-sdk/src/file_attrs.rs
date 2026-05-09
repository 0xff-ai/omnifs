use crate::error::{ProviderError, Result};
use crate::omnifs::provider::types as wit_types;

pub const MAX_PROJECTED_BYTES: usize = 64 * 1024;
pub const MAX_EAGER_RESPONSE_BYTES: usize = 512 * 1024;
pub const MAX_VERSION_TOKEN_BYTES: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileAttrs {
    pub size: Size,
    pub bytes: Bytes,
    pub stability: Stability,
    pub version: Option<VersionToken>,
}

impl FileAttrs {
    pub fn inline(
        bytes: impl Into<Vec<u8>>,
        stability: Stability,
        version: Option<VersionToken>,
    ) -> Self {
        let bytes = bytes.into();
        Self {
            size: Size::Exact(u64::try_from(bytes.len()).unwrap_or(u64::MAX)),
            bytes: Bytes::Inline(bytes),
            stability,
            version,
        }
    }

    pub fn deferred(size: Size, read: ReadMode, stability: Stability) -> Self {
        Self {
            size,
            bytes: Bytes::Deferred { read },
            stability,
            version: None,
        }
    }

    #[must_use]
    pub fn with_version(mut self, version: impl Into<VersionToken>) -> Self {
        self.version = Some(version.into());
        self
    }

    pub fn st_size(&self) -> u64 {
        self.size.st_size()
    }

    pub fn inline_bytes(&self) -> Option<&[u8]> {
        match &self.bytes {
            Bytes::Inline(bytes) => Some(bytes),
            Bytes::Deferred { .. } => None,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.stability == Stability::Volatile
            && !matches!(
                self.bytes,
                Bytes::Deferred {
                    read: ReadMode::Ranged
                }
            )
        {
            return Err(ProviderError::invalid_input(
                "Stability::Volatile requires Bytes::Deferred { read: ReadMode::Ranged }",
            ));
        }

        match (&self.bytes, &self.size) {
            (Bytes::Inline(bytes), Size::Exact(size)) => {
                let len = u64::try_from(bytes.len())
                    .map_err(|_| ProviderError::too_large("inline file length does not fit u64"))?;
                if *size != len {
                    return Err(ProviderError::invalid_input(format!(
                        "inline file declares size {size} but carries {len} bytes"
                    )));
                }
                if bytes.len() > MAX_PROJECTED_BYTES {
                    return Err(ProviderError::too_large(format!(
                        "projected file exceeds eager byte limit of {MAX_PROJECTED_BYTES} bytes"
                    )));
                }
            },
            (Bytes::Inline(_), Size::NonZero | Size::Unknown) => {
                return Err(ProviderError::invalid_input(
                    "inline bytes require Size::Exact(bytes.len())",
                ));
            },
            (Bytes::Deferred { .. }, _) => {},
        }

        if let Some(version) = &self.version {
            version.validate()?;
        }

        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionToken(pub String);

impl VersionToken {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn validate(&self) -> Result<()> {
        if self.0.is_empty() {
            return Err(ProviderError::invalid_input(
                "version token must not be empty",
            ));
        }
        if self.0.len() > MAX_VERSION_TOKEN_BYTES {
            return Err(ProviderError::invalid_input(format!(
                "version token exceeds {MAX_VERSION_TOKEN_BYTES} bytes"
            )));
        }
        Ok(())
    }
}

impl From<String> for VersionToken {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for VersionToken {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Size {
    Exact(u64),
    NonZero,
    Unknown,
}

impl Size {
    pub fn st_size(&self) -> u64 {
        match self {
            Self::Exact(size) => *size,
            Self::NonZero => 1,
            Self::Unknown => 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Bytes {
    Inline(Vec<u8>),
    Deferred { read: ReadMode },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadMode {
    Full,
    Ranged,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stability {
    Immutable,
    Mutable,
    Volatile,
}

impl From<Size> for wit_types::FileSize {
    fn from(size: Size) -> Self {
        match size {
            Size::Exact(size) => Self::Exact(size),
            Size::NonZero => Self::NonZero,
            Size::Unknown => Self::Unknown,
        }
    }
}

impl From<ReadMode> for wit_types::ReadMode {
    fn from(mode: ReadMode) -> Self {
        match mode {
            ReadMode::Full => Self::Full,
            ReadMode::Ranged => Self::Ranged,
        }
    }
}

impl From<Bytes> for wit_types::FileBytes {
    fn from(bytes: Bytes) -> Self {
        match bytes {
            Bytes::Inline(bytes) => Self::Inline(bytes),
            Bytes::Deferred { read } => Self::Deferred(read.into()),
        }
    }
}

impl From<Stability> for wit_types::Stability {
    fn from(stability: Stability) -> Self {
        match stability {
            Stability::Immutable => Self::Immutable,
            Stability::Mutable => Self::Mutable,
            Stability::Volatile => Self::Volatile,
        }
    }
}

impl From<FileAttrs> for wit_types::FileAttrs {
    fn from(attrs: FileAttrs) -> Self {
        Self {
            size: attrs.size.into(),
            bytes: attrs.bytes.into(),
            stability: attrs.stability.into(),
            version_token: attrs.version.map(|version| version.0),
        }
    }
}
