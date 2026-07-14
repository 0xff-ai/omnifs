//! Generated bindings and package identity for the omnifs WIT contract.

/// Package declaration from `wit/provider.wit`.
pub const PROVIDER_WIT_PACKAGE: &str = "package omnifs:provider@0.6.0;";

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
