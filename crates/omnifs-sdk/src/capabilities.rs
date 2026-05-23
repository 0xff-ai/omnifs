//! Helpers for provider runtime capability requests.

use crate::omnifs::provider::types::RequestedCapabilities;

impl RequestedCapabilities {
    /// Runtime-only capability request with no install-time metadata duplication.
    pub fn runtime_only(refresh_interval_secs: u32) -> Self {
        Self {
            refresh_interval_secs,
            ..Self::empty()
        }
    }

    /// Runtime-only request that also needs git clone callouts.
    pub fn with_git(refresh_interval_secs: u32) -> Self {
        Self {
            needs_git: true,
            refresh_interval_secs,
            ..Self::empty()
        }
    }

    pub fn empty() -> Self {
        Self {
            domains: Vec::new(),
            unix_sockets: Vec::new(),
            auth_types: Vec::new(),
            max_memory_mb: 0,
            needs_git: false,
            needs_websocket: false,
            needs_streaming: false,
            refresh_interval_secs: 0,
        }
    }
}
