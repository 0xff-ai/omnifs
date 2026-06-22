//! Host-side capability enforcement.
//!
//! The capability model, the matching rules, and the per-callout decision live
//! in [`omnifs_caps`]; this module is the enforcement seam: it resolves a
//! mounted provider's grants into a runtime [`Allowlist`] (resolving dynamic
//! socket grants from the mount's endpoint) and gates every provider callout
//! through it.

use std::path::PathBuf;

use omnifs_caps::{Allowlist, Error, Grant};
use omnifs_mount::mounts::Resolved;
use omnifs_wit::provider::types as wit_types;

/// Default sandbox memory budget when a mount grants no explicit limit.
const DEFAULT_MAX_MEMORY_MB: u32 = 64;

/// Wraps the resolved [`Allowlist`] and gates provider callouts against it. The
/// decision logic is `omnifs-caps`; this type is where the host enforces it.
pub struct CapabilityChecker {
    grants: Allowlist,
}

impl CapabilityChecker {
    #[must_use]
    pub fn new(grants: Allowlist) -> Self {
        Self { grants }
    }

    /// Build the enforcement allowlist from a resolved mount's grants plus the
    /// provider's runtime-requested capabilities. A dynamic unix-socket grant is
    /// resolved from the mount's `endpoint` config here; a malformed endpoint
    /// resolves to no socket, so the provider is simply denied at callout time.
    #[must_use]
    pub fn from_config(
        config: &Resolved,
        provider_caps: &wit_types::RequestedCapabilities,
    ) -> Self {
        Self::new(allowlist_from_config(config, provider_caps))
    }

    #[must_use]
    pub fn grants(&self) -> &Allowlist {
        &self.grants
    }

    pub fn check_url(&self, url: &str) -> Result<(), Error> {
        self.grants.check_url(url)
    }

    pub fn check_git_url(&self, url: &str) -> Result<(), Error> {
        self.grants.check_git_url(url)
    }

    /// Decode the socket path from a `unix:` URL without allowlist checks; the
    /// executor opens it once [`check_url`](Self::check_url) has approved it.
    pub fn decode_unix_socket(url: &str) -> Result<PathBuf, Error> {
        Allowlist::decode_unix_socket(url)
    }
}

fn allowlist_from_config(
    config: &Resolved,
    provider_caps: &wit_types::RequestedCapabilities,
) -> Allowlist {
    let grants = config.spec.capabilities.as_ref();

    let mut unix_sockets: Vec<PathBuf> = match grants.and_then(|g| g.unix_sockets.as_ref()) {
        Some(Grant::Literal(paths)) => paths.iter().map(PathBuf::from).collect(),
        Some(Grant::Dynamic(_)) => endpoint(config)
            .and_then(|endpoint| omnifs_caps::endpoint_socket(endpoint).ok().flatten())
            .map(PathBuf::from)
            .into_iter()
            .collect(),
        None => Vec::new(),
    };
    unix_sockets.extend(provider_caps.unix_sockets.iter().map(PathBuf::from));
    unix_sockets.sort();
    unix_sockets.dedup();

    Allowlist {
        domains: literal(grants.and_then(|g| g.domains.as_ref())),
        git_repos: literal(grants.and_then(|g| g.git_repos.as_ref())),
        max_memory_mb: grants
            .and_then(|g| g.max_memory_mb)
            .unwrap_or(DEFAULT_MAX_MEMORY_MB),
        needs_git: provider_caps.needs_git,
        unix_sockets,
    }
}

/// The literal values of a string grant, or empty for a dynamic or absent one.
fn literal(grant: Option<&Grant<String>>) -> Vec<String> {
    match grant {
        Some(Grant::Literal(values)) => values.clone(),
        _ => Vec::new(),
    }
}

fn endpoint(config: &Resolved) -> Option<&str> {
    config
        .spec
        .config_raw
        .as_ref()
        .and_then(|config| config.as_value().get("endpoint"))
        .and_then(serde_json::Value::as_str)
}
