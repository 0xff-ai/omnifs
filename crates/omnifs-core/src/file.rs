/// Declared file size: an exact byte length, a known non-empty value with an
/// unknown length, or no length information.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FileSize {
    Exact(u64),
    NonZero,
    Unknown,
}

impl FileSize {
    /// The `st_size` value to report before the exact length is learned.
    ///
    /// Unknown and non-empty files use the smallest non-zero sentinel so
    /// stat-driven tools do not mistake them for empty files.
    #[must_use]
    pub fn st_size(self) -> u64 {
        match self {
            Self::Exact(size) => size,
            Self::NonZero | Self::Unknown => 1,
        }
    }
}

/// How a provider can serve deferred file content.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ReadMode {
    Full,
    Ranged,
}

/// How file bytes behave over time for one logical identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Stability {
    /// Bytes do not change for this identity.
    Stable,
    /// Bytes may change between observations, but not during one observation.
    Dynamic,
    /// Bytes may change while they are being observed.
    Live,
}
