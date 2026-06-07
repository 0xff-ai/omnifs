use crate::error::{ProviderError, Result};
use omnifs_core::ContentType;
use omnifs_wit::provider::types as wit_types;

pub const MAX_PROJECTED_BYTES: usize = 64 * 1024;
pub const MAX_EAGER_RESPONSE_BYTES: usize = 512 * 1024;
pub const MAX_VERSION_TOKEN_BYTES: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileAttrs {
    pub size: Size,
    pub stability: Stability,
    pub version: Option<VersionToken>,
}

impl FileAttrs {
    pub fn new(size: Size, stability: Stability) -> Self {
        Self {
            size,
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

    pub fn validate(&self) -> Result<()> {
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
            Self::NonZero | Self::Unknown => 1,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileProj {
    pub attrs: FileAttrs,
    pub bytes: ProjBytes,
    pub content_type: Option<ContentType>,
}

impl FileProj {
    pub fn inline(
        bytes: impl Into<Vec<u8>>,
        stability: Stability,
        version: Option<VersionToken>,
    ) -> Self {
        let bytes = bytes.into();
        Self {
            attrs: FileAttrs {
                size: Size::Exact(u64::try_from(bytes.len()).unwrap_or(u64::MAX)),
                stability,
                version,
            },
            bytes: ProjBytes::Inline(bytes),
            content_type: None,
        }
    }

    pub fn deferred(size: Size, read: ReadMode, stability: Stability) -> Self {
        Self {
            attrs: FileAttrs::new(size, stability),
            bytes: ProjBytes::Deferred { read },
            content_type: None,
        }
    }

    /// Directory listing entry: attrs for `stat`/`ls` without inline bytes; content
    /// is loaded on `read-file` or via an object/canonical path.
    pub fn listing_shape() -> Self {
        Self::deferred(Size::Unknown, ReadMode::Full, Stability::Immutable)
    }

    #[must_use]
    pub fn with_version(mut self, version: impl Into<VersionToken>) -> Self {
        self.attrs.version = Some(version.into());
        self
    }

    #[must_use]
    pub fn with_content_type(mut self, content_type: ContentType) -> Self {
        self.content_type = Some(content_type);
        self
    }

    pub fn validate(&self) -> Result<()> {
        self.attrs.validate()?;

        if self.attrs.stability == Stability::Volatile
            && !matches!(
                self.bytes,
                ProjBytes::Deferred {
                    read: ReadMode::Ranged
                }
            )
        {
            return Err(ProviderError::invalid_input(
                "Stability::Volatile requires ProjBytes::Deferred { read: ReadMode::Ranged }",
            ));
        }

        match (&self.bytes, &self.attrs.size) {
            (ProjBytes::Inline(bytes), Size::Exact(size)) => {
                let len = u64::try_from(bytes.len())
                    .map_err(|_| ProviderError::too_large("inline file length does not fit u64"))?;
                if *size != len {
                    return Err(ProviderError::invalid_input(format!(
                        "inline projection declares size {size} but carries {len} bytes"
                    )));
                }
                if bytes.len() > MAX_PROJECTED_BYTES {
                    return Err(ProviderError::too_large(format!(
                        "inline projection exceeds eager byte limit of {MAX_PROJECTED_BYTES} bytes"
                    )));
                }
            },
            (ProjBytes::Inline(_), Size::NonZero | Size::Unknown) => {
                return Err(ProviderError::invalid_input(
                    "inline projection bytes require Size::Exact(bytes.len())",
                ));
            },
            (ProjBytes::Deferred { .. }, _) => {},
        }

        Ok(())
    }

    pub fn inline_bytes(&self) -> Option<&[u8]> {
        match &self.bytes {
            ProjBytes::Inline(bytes) => Some(bytes),
            ProjBytes::Deferred { .. } => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjBytes {
    Inline(Vec<u8>),
    Deferred { read: ReadMode },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadFileBytes {
    Inline(Vec<u8>),
    Blob(crate::blob::BlobId),
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

impl From<ProjBytes> for wit_types::ByteSource {
    fn from(bytes: ProjBytes) -> Self {
        match bytes {
            ProjBytes::Inline(bytes) => Self::Inline(bytes),
            ProjBytes::Deferred { read } => Self::Deferred(read.into()),
        }
    }
}

impl From<ReadFileBytes> for wit_types::ByteSource {
    fn from(bytes: ReadFileBytes) -> Self {
        match bytes {
            ReadFileBytes::Inline(bytes) => Self::Inline(bytes),
            ReadFileBytes::Blob(blob) => Self::Blob(blob.raw()),
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
            stability: attrs.stability.into(),
            version_token: attrs.version.map(|version| version.0),
        }
    }
}

impl From<FileProj> for wit_types::FileOut {
    fn from(file: FileProj) -> Self {
        Self {
            content_type: file
                .content_type
                .map(|content_type| content_type.as_mime().to_string()),
            attrs: file.attrs.into(),
            bytes: file.bytes.into(),
        }
    }
}
