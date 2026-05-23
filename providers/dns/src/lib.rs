#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use std::collections::BTreeMap;

pub(crate) use omnifs_sdk::prelude::Result;

mod doh;
mod http_ext;
mod provider;
mod query;
mod root;
mod segment;
pub(crate) mod types;

#[derive(Clone)]
pub(crate) struct State {
    pub resolvers: doh::ResolverConfig,
}

#[derive(Clone, Debug)]
pub(crate) struct DnsRecord {
    pub rtype: types::SupportedRecordType,
    pub value: String,
}

#[omnifs_sdk::config]
struct Config {
    #[serde(default = "default_resolver_name")]
    default_resolver: String,
    #[serde(default)]
    resolvers: BTreeMap<String, ConfigResolver>,
}

fn default_resolver_name() -> String {
    String::from("cloudflare")
}

#[omnifs_sdk::config]
struct ConfigResolver {
    url: String,
    #[serde(default)]
    aliases: Vec<String>,
}
