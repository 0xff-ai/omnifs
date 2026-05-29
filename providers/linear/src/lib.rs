#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

//! linear-provider: Linear virtual filesystem provider for omnifs.
//!
//! Exposes Linear teams and issues as a virtual filesystem using the
//! omnifs provider WIT interface. The current v1 surface covers:
//!
//! - workspace root listing all teams by key
//! - per-team `issues/open` and `issues/all` filtered listings
//! - per-issue projection (`title`, `state`, `priority`, `assignee`,
//!   `description.md`)
//!
//! Mutability is modeled with `Stability::Mutable` and version tokens
//! derived from each issue's `updatedAt` timestamp. Listings preload
//! the per-issue files so simple browsing avoids per-issue round trips.

pub(crate) use omnifs_sdk::prelude::Result;

#[allow(dead_code)]
mod graphql;
mod http_ext;
mod issue_subtree;
mod issues;
mod provider;
mod root;
mod teams;
mod types;

/// Linear's GraphQL endpoint. All callouts target this URL.
pub(crate) const API_ENDPOINT: &str = "https://api.linear.app/graphql";

#[derive(Clone, Default)]
#[omnifs_sdk::config]
pub struct Config {}

#[derive(Clone, Default)]
pub struct State {
    pub config: Config,
}
