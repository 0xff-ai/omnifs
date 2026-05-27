//! Library surface for omnifs-cli. Exposes the modules that integration
//! tests under `tests/` need to reach by name; the binary uses the same
//! modules directly via `mod` declarations in `main.rs`.

mod builtin_catalog;
#[allow(dead_code)]
mod catalog;
pub mod config;
#[doc(hidden)]
pub mod inspector;
pub mod paths;
