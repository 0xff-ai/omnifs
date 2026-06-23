//! Reusable host-side support for one-shot sandboxed tools.
//!
//! This module contains mechanics that are independent of any specific
//! tool: staging narrow filesystem capabilities, publishing completed
//! output trees, and caching materialized trees by semantic view key.

pub mod preopen;
pub mod publish;
pub(crate) mod relative_key;
