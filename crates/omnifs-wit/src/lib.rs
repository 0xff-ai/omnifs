//! Generated bindings and package identity for the omnifs WIT contract.
//!
//! Guest (`wit-bindgen`) and host (`wasmtime`) surfaces coexist. Guest bindings
//! always compile under [`provider`]; host bindings compile under [`host`] when
//! the `host-bindings` feature is enabled. They are never alternates of one
//! module, so Cargo feature unification cannot displace the guest `Guest`
//! traits while a host crate enables `host-bindings`.

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

/// Guest bindings for the `omnifs:provider` world (`wit-bindgen`).
///
/// Providers and `omnifs-sdk` always use this module.
#[allow(clippy::same_length_and_capacity, clippy::unsafe_derive_deserialize)]
pub mod provider {
    wit_bindgen::generate!({
        world: "provider",
        path: "wit",
        pub_export_macro: true,
        generate_unused_types: true,
        additional_derives: [Clone, serde::Serialize, serde::Deserialize],
    });

    pub use omnifs::provider::types;

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

/// Host bindings for the `omnifs:provider` world (`wasmtime::component::bindgen`).
///
/// Engine, itest, and other Wasmtime hosts use this module. Enabled only with
/// the `host-bindings` feature; guest bindings in [`provider`] remain available.
#[cfg(feature = "host-bindings")]
#[allow(clippy::same_length_and_capacity, clippy::unsafe_derive_deserialize)]
pub mod host {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "provider",
        additional_derives: [Clone, serde::Serialize, serde::Deserialize],
    });

    pub use omnifs::provider::log;
    pub use omnifs::provider::types;
}

macro_rules! wit_core_conversions {
    ($module:ident) => {
        impl From<omnifs_core::FileSize> for $module::types::FileSize {
            fn from(size: omnifs_core::FileSize) -> Self {
                match size {
                    omnifs_core::FileSize::Exact(size) => Self::Exact(size),
                    omnifs_core::FileSize::NonZero => Self::NonZero,
                    omnifs_core::FileSize::Unknown => Self::Unknown,
                }
            }
        }

        impl From<$module::types::FileSize> for omnifs_core::FileSize {
            fn from(size: $module::types::FileSize) -> Self {
                match size {
                    $module::types::FileSize::Exact(size) => Self::Exact(size),
                    $module::types::FileSize::NonZero => Self::NonZero,
                    $module::types::FileSize::Unknown => Self::Unknown,
                }
            }
        }

        impl From<omnifs_core::ReadMode> for $module::types::ReadMode {
            fn from(mode: omnifs_core::ReadMode) -> Self {
                match mode {
                    omnifs_core::ReadMode::Full => Self::Full,
                    omnifs_core::ReadMode::Ranged => Self::Ranged,
                }
            }
        }

        impl From<$module::types::ReadMode> for omnifs_core::ReadMode {
            fn from(mode: $module::types::ReadMode) -> Self {
                match mode {
                    $module::types::ReadMode::Full => Self::Full,
                    $module::types::ReadMode::Ranged => Self::Ranged,
                }
            }
        }

        impl From<omnifs_core::Stability> for $module::types::Stability {
            fn from(stability: omnifs_core::Stability) -> Self {
                match stability {
                    omnifs_core::Stability::Stable => Self::Stable,
                    omnifs_core::Stability::Dynamic => Self::Dynamic,
                    omnifs_core::Stability::Live => Self::Live,
                }
            }
        }

        impl From<$module::types::Stability> for omnifs_core::Stability {
            fn from(stability: $module::types::Stability) -> Self {
                match stability {
                    $module::types::Stability::Stable => Self::Stable,
                    $module::types::Stability::Dynamic => Self::Dynamic,
                    $module::types::Stability::Live => Self::Live,
                }
            }
        }
    };
}

wit_core_conversions!(provider);

#[cfg(feature = "host-bindings")]
wit_core_conversions!(host);
