//! Reusable host-side support for materialized output trees.
//!
//! This module contains mechanics that are independent of any specific
//! source: publishing completed output trees by atomic rename, and
//! caching materialized trees by semantic view key.

pub mod publish;
