//! Source locations and generated bindings for omnifs WIT packages.

use std::path::Path;

const PROVIDER_WORLD_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/wit");

/// Package declaration from `wit/provider.wit`.
pub const PROVIDER_WIT_PACKAGE: &str = "package omnifs:provider@0.4.0;";

/// Return the directory containing the `omnifs:provider` WIT package.
pub fn provider_world_path() -> &'static Path {
    Path::new(PROVIDER_WORLD_DIR)
}

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

    impl types::ProviderReturn {
        /// Terminal return with no host-side effects.
        #[must_use]
        pub fn terminal(result: types::OpResult) -> Self {
            Self {
                result,
                effects: types::Effects {
                    canonical: Vec::new(),
                    fs: Vec::new(),
                    invalidations: Vec::new(),
                },
            }
        }

        /// Terminal return with effects committed if the return is accepted.
        #[must_use]
        pub fn with_effects(result: types::OpResult, effects: types::Effects) -> Self {
            Self { result, effects }
        }

        /// Unwrap the operation result. Intended for test assertions.
        #[must_use]
        pub fn expect_result(self) -> types::OpResult {
            self.result
        }
    }

    impl types::RequestedCapabilities {
        /// Runtime-only capability request with no install-time metadata duplication.
        #[must_use]
        pub fn runtime_only(refresh_interval_secs: u32) -> Self {
            Self {
                refresh_interval_secs,
                ..Self::empty()
            }
        }

        /// Runtime-only request that also needs git clone callouts.
        #[must_use]
        pub fn with_git(refresh_interval_secs: u32) -> Self {
            Self {
                needs_git: true,
                refresh_interval_secs,
                ..Self::empty()
            }
        }

        /// Empty runtime capability request.
        #[must_use]
        pub fn empty() -> Self {
            Self {
                domains: Vec::new(),
                unix_sockets: Vec::new(),
                auth_types: Vec::new(),
                max_memory_mb: 0,
                needs_git: false,
                needs_websocket: false,
                needs_streaming: false,
                refresh_interval_secs: 0,
            }
        }
    }

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
