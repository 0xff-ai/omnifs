//! Provider initialization builder.
//!
//! [`Init`] pairs a provider's initial typed state with the `ProviderInfo`
//! metadata (name, version, optional description) reported to the host at
//! startup. It is a plain data builder: construct with [`Init::new`],
//! optionally chain [`Init::description`], and split with
//! [`Init::into_parts`].

use omnifs_wit::provider::types::ProviderInfo;

/// A provider's initial state bundled with its `ProviderInfo` metadata.
pub struct Init<S> {
    state: S,
    info: ProviderInfo,
}

impl<S> Init<S> {
    /// Bundle the initial state with a name and version; the description
    /// starts empty.
    pub fn new(state: S, name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            state,
            info: ProviderInfo {
                name: name.into(),
                version: version.into(),
                description: String::new(),
                // Populated from the sealed router by the `#[provider]` glue;
                // a bare `Init` carries no routes.
                routes: Vec::new(),
            },
        }
    }

    /// Set the provider description.
    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.info.description = description.into();
        self
    }

    /// Split into the state and `ProviderInfo` halves.
    pub fn into_parts(self) -> (S, ProviderInfo) {
        (self.state, self.info)
    }
}
