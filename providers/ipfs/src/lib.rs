#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

pub(crate) use omnifs_sdk::prelude::Result;

mod api;
pub mod meta;
mod provider;
pub mod root;
pub(crate) mod types;

#[derive(Clone)]
pub(crate) struct State {
    pub(crate) config: Config,
}

#[derive(Clone)]
#[omnifs_sdk::config]
pub struct Config {
    #[serde(default = "default_api_base_url")]
    pub(crate) api_base_url: String,
    #[serde(default = "default_ipns_resolve_timeout_secs")]
    pub(crate) ipns_resolve_timeout_secs: u64,
    #[serde(default = "default_enumerate_pins")]
    pub(crate) enumerate_pins: bool,
    #[serde(default = "default_enumerate_keys")]
    pub(crate) enumerate_keys: bool,
}

fn default_api_base_url() -> String {
    String::from("http://127.0.0.1:5001/api/v0")
}

fn default_ipns_resolve_timeout_secs() -> u64 {
    30
}

fn default_enumerate_pins() -> bool {
    true
}

fn default_enumerate_keys() -> bool {
    true
}

#[cfg(test)]
mod registry_smoke {
    use super::*;
    use crate::meta::MetaHandlers;
    use crate::root::RootHandlers;
    use omnifs_sdk::__internal::MountRegistry;

    // The provider's path manifest must validate end-to-end. This is the
    // first dir+file co-existence on identical rest-captured templates;
    // the SDK's MountRegistry::validate change in feat/sdk-handlers-coexist
    // is what makes it accept.
    #[test]
    fn manifest_validates() {
        let mut registry: MountRegistry<State> = MountRegistry::new();
        RootHandlers::mount(&mut registry);
        MetaHandlers::mount(&mut registry);
        registry.validate().expect("manifest should validate");
    }
}
