#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

//! arxiv-provider: arXiv virtual filesystem provider for omnifs.
//!
//! Mirrors arXiv into a projected filesystem: browse papers by
//! `categories/{cat}/{YYYY}/{MM}/{DD}/`, `authors/{author}/`, or
//! `search/{query}/`, plus `new/{N}` and `updated/{N}` windowed
//! scrolls under each scope. Each paper exposes its PDF, tarball
//! source, `metadata.json`, and `links.json`, with per-version
//! variants under `versions/{vN}/`.

pub(crate) use omnifs_sdk::prelude::Result;

mod api;
mod authors;
mod categories;
mod http_ext;
mod paper;
mod paper_subtree;
mod papers;
mod provider;
mod query;
mod root;
mod search;
mod selector;
pub(crate) mod types;

#[derive(Clone)]
#[omnifs_sdk::config]
pub struct Config {}

#[derive(Clone)]
pub struct State {
    pub config: Config,
}
