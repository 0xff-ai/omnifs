//! Generated bindings and package identity for the omnifs WIT contract.

/// Package declaration from `wit/provider.wit`.
pub const PROVIDER_WIT_PACKAGE: &str = "package omnifs:provider@0.7.0;";

#[cfg(test)]
mod tests {
    #[test]
    fn provider_wit_package_constant_matches_wit_file() {
        let package_line = include_str!("../wit/provider.wit")
            .lines()
            .next()
            .expect("provider.wit has a package line");
        assert_eq!(super::PROVIDER_WIT_PACKAGE, package_line);
    }
}

/// Generated bindings for the `omnifs:provider` package.
#[allow(clippy::same_length_and_capacity, clippy::unsafe_derive_deserialize)]
pub mod provider {
    #[cfg(not(feature = "host-bindings"))]
    wit_bindgen::generate!({
        world: "provider",
        path: "wit",
        pub_export_macro: true,
        generate_unused_types: true,
        additional_derives: [Clone, serde::Serialize, serde::Deserialize],
    });

    #[cfg(feature = "host-bindings")]
    wasmtime::component::bindgen!({
        path: "wit",
        world: "provider",
        additional_derives: [Clone, serde::Serialize, serde::Deserialize],
    });

    pub use omnifs::provider::types;

    #[cfg(feature = "host-bindings")]
    pub use omnifs::provider::log;

    impl types::ProviderEvent {
        /// The kebab-case label of this variant, matching the `provider-event`
        /// cases in the `omnifs:provider` WIT.
        #[must_use]
        pub fn name(&self) -> &'static str {
            match self {
                types::ProviderEvent::FileChanged(_) => "file-changed",
                types::ProviderEvent::WebhookReceived(_) => "webhook-received",
                types::ProviderEvent::TimerTick => "timer-tick",
                types::ProviderEvent::AuthRefreshed => "auth-refreshed",
            }
        }
    }
}

impl From<omnifs_core::FileSize> for provider::types::FileSize {
    fn from(size: omnifs_core::FileSize) -> Self {
        match size {
            omnifs_core::FileSize::Exact(size) => Self::Exact(size),
            omnifs_core::FileSize::NonZero => Self::NonZero,
            omnifs_core::FileSize::Unknown => Self::Unknown,
        }
    }
}

impl From<provider::types::FileSize> for omnifs_core::FileSize {
    fn from(size: provider::types::FileSize) -> Self {
        match size {
            provider::types::FileSize::Exact(size) => Self::Exact(size),
            provider::types::FileSize::NonZero => Self::NonZero,
            provider::types::FileSize::Unknown => Self::Unknown,
        }
    }
}

impl From<omnifs_core::ReadMode> for provider::types::ReadMode {
    fn from(mode: omnifs_core::ReadMode) -> Self {
        match mode {
            omnifs_core::ReadMode::Full => Self::Full,
            omnifs_core::ReadMode::Ranged => Self::Ranged,
        }
    }
}

impl From<provider::types::ReadMode> for omnifs_core::ReadMode {
    fn from(mode: provider::types::ReadMode) -> Self {
        match mode {
            provider::types::ReadMode::Full => Self::Full,
            provider::types::ReadMode::Ranged => Self::Ranged,
        }
    }
}

impl From<omnifs_core::Stability> for provider::types::Stability {
    fn from(stability: omnifs_core::Stability) -> Self {
        match stability {
            omnifs_core::Stability::Stable => Self::Stable,
            omnifs_core::Stability::Dynamic => Self::Dynamic,
            omnifs_core::Stability::Live => Self::Live,
        }
    }
}

impl From<provider::types::Stability> for omnifs_core::Stability {
    fn from(stability: provider::types::Stability) -> Self {
        match stability {
            provider::types::Stability::Stable => Self::Stable,
            provider::types::Stability::Dynamic => Self::Dynamic,
            provider::types::Stability::Live => Self::Live,
        }
    }
}
