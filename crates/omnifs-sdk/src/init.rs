//! Provider initialization builder.
//!
//! Provides a typed builder for constructing the initial provider state
//! and metadata returned from `init`.

use omnifs_wit::provider::types::ProviderInfo;

/// Initialization context for a provider.
///
/// This type wraps the provider state and metadata, allowing a builder-style
/// API for setting the provider name, version, and description before
/// returning from `init`.
pub struct Init<S> {
    state: S,
    info: ProviderInfo,
}

impl<S> Init<S> {
    /// Create a new initialization context with the given state and metadata.
    pub fn new(state: S, name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            state,
            info: ProviderInfo {
                name: name.into(),
                version: version.into(),
                description: String::new(),
            },
        }
    }

    /// Set the provider description.
    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.info.description = description.into();
        self
    }

    /// Consume the Init and return the state and provider info parts.
    ///
    /// This is used by the macro to extract the values to return from
    /// the generated `init` function.
    pub fn into_parts(self) -> (S, ProviderInfo) {
        (self.state, self.info)
    }
}
