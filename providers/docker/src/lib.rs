#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

//! docker-provider: Docker daemon virtual filesystem provider for
//! omnifs.
//!
//! Mirrors a local Docker daemon into a projected filesystem rooted
//! at `/docker`. Phase 2 ships `/system`, `/containers`, and a
//! minimal `/compose` slice; the rest of the path tree is Phase 3.

pub(crate) use omnifs_sdk::prelude::Result;

mod api;
mod compose;
mod container_subtree;
mod containers;
mod events;
mod provider;
mod system;
mod types;
mod wire;

use api::ApiBase;

#[derive(Clone)]
#[omnifs_sdk::config]
pub struct Config {
    /// Endpoint URL the provider talks to. `unix:///path/to/sock`
    /// dispatches over a unix socket; an `http(s)://` URL uses
    /// regular HTTP transport. Phase 2 only exercises the unix
    /// path; remote TCP support is a future capability gated on
    /// auth and TLS choices.
    #[serde(default = "default_endpoint")]
    endpoint: String,
}

fn default_endpoint() -> String {
    "unix:///var/run/docker.sock".to_string()
}

#[derive(Clone)]
pub struct State {
    pub config: Config,
    pub api: ApiBase,
    pub events: EventCheckpoint,
}

impl State {
    /// Wall-clock seconds since epoch. WASI's `clock_time_get` is
    /// reachable through `std::time::SystemTime`, but the surface we
    /// want is bare `u64`; the helper centralises the conversion so
    /// callers don't keep importing `SystemTime` and `UNIX_EPOCH`.
    pub fn clock_now_secs(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}

/// Tracks the `since` cursor for the docker `/events` poll. The first
/// tick records the wall-clock time as both `since` and `until`; every
/// subsequent tick advances the cursor to the previous `until`. On a
/// failed tick we rewind to the previous cursor so events are not
/// silently dropped.
#[derive(Clone, Debug, Default)]
pub struct EventCheckpoint {
    /// Last known `until` we successfully polled to, in epoch seconds.
    /// `None` until the first tick lands.
    last_until: Option<u64>,
}

impl EventCheckpoint {
    /// Compute the `since` value for a tick whose `until` is `now`,
    /// then optimistically advance the cursor.
    pub fn checkpoint(&mut self, now: u64) -> u64 {
        let since = self.last_until.unwrap_or(now);
        self.last_until = Some(now);
        since
    }

    pub fn rewind(&mut self, previous_since: u64) {
        self.last_until = Some(previous_since);
    }
}
