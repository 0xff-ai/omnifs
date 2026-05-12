#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

//! arxiv-provider: arXiv virtual filesystem provider for omnifs.
//!
//! Projects recent arXiv category submissions and direct paper lookups.
//! Category traversal is rooted at `categories/{cat}/recent` and
//! materialized submission-day buckets under `categories/{cat}/submissions`.

pub(crate) use omnifs_sdk::prelude::Result;
use std::collections::HashMap;

mod api;
mod categories;
mod paper;
mod provider;
mod recent;
pub(crate) mod types;

#[derive(Clone, Default)]
#[omnifs_sdk::config]
pub struct Config {}

#[derive(Clone, Default)]
pub struct State {
    pub config: Config,
    pub(crate) recent: HashMap<types::CategoryKey, recent::RecentIndex>,
}
