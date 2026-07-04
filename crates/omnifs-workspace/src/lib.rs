//! Every byte under `OMNIFS_HOME` has its format owned here: the directory
//! layout, mount specs, the provider index, and credential stores.

#![forbid(unsafe_code)]

pub mod authn;
pub mod creds;
pub mod ids;
pub mod io;
pub mod layout;
pub mod mounts;
pub mod provider;
pub mod telemetry;
