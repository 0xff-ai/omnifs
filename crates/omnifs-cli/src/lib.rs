//! Library surface for omnifs-cli. Exposes the modules that integration
//! tests under `tests/` need to reach by name; the binary uses the same
//! modules directly via `mod` declarations in `main.rs`.

pub mod config;
#[doc(hidden)]
pub mod inspector;
pub mod paths;
