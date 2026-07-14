//! Host-side capability enforcement.
//!
//! The capability model, the matching rules, and the per-callout decision live
//! in [`omnifs_caps`]; this module is the enforcement seam: it resolves a
//! mounted provider's grants into a runtime [`Allowlist`] (resolving dynamic
//! domain and socket grants from provider config fields) and gates every
//! provider callout through it.

use std::path::PathBuf;

use omnifs_caps::{Allowlist, Error, Grant};
use omnifs_wit::provider::types as wit_types;
use omnifs_workspace::mounts::Spec;
use omnifs_workspace::provider::ConfigMetadata;

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

    /// Build the enforcement allowlist from a mount spec's grants. Provider
    /// runtime output may narrow use through `needs_git`, but it cannot add
    /// grant values. A dynamic domain grant is resolved from a `domains`
    /// string-array config field. A dynamic unix-socket grant is resolved from
    /// the config field the provider marks as a host socket. A malformed or
    /// missing value resolves to no grant, so the provider is simply denied at
    /// callout time.
    #[must_use]
    pub fn from_config(
        config: &Spec,
        provider_caps: &wit_types::RequestedCapabilities,
        metadata: Option<&ConfigMetadata>,
    ) -> Self {
        Self::new(allowlist_from_config(config, provider_caps, metadata))
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
    config: &Spec,
    provider_caps: &wit_types::RequestedCapabilities,
    metadata: Option<&ConfigMetadata>,
) -> Allowlist {
    let grants = config.capabilities.as_ref();

    // Initialize output is untrusted and cannot widen the spec-owned socket
    // grant, so `provider_caps.unix_sockets` is deliberately ignored.
    let unix_sockets: Vec<PathBuf> = match grants.and_then(|g| g.unix_sockets.as_ref()) {
        Some(Grant::Literal(paths)) => paths.iter().map(PathBuf::from).collect(),
        Some(Grant::Dynamic(_)) => dynamic_socket(config, metadata).into_iter().collect(),
        None => Vec::new(),
    };

    Allowlist {
        domains: domains(config, grants.and_then(|g| g.domains.as_ref()), metadata),
        git_repos: literal(grants.and_then(|g| g.git_repos.as_ref())),
        needs_git: provider_caps.needs_git,
        unix_sockets,
    }
}

fn domains(
    config: &Spec,
    grant: Option<&Grant<String>>,
    metadata: Option<&ConfigMetadata>,
) -> Vec<String> {
    match grant {
        Some(Grant::Literal(values)) => values.clone(),
        Some(Grant::Dynamic(_)) => dynamic_domains(config, metadata),
        None => Vec::new(),
    }
}

/// The literal values of a string grant, or empty for a dynamic or absent one.
fn literal(grant: Option<&Grant<String>>) -> Vec<String> {
    match grant {
        Some(Grant::Literal(values)) => values.clone(),
        _ => Vec::new(),
    }
}

/// The host socket a dynamic unix-socket grant resolves to: the `unix://`
/// endpoint in the config field the provider marks as a host socket.
fn dynamic_socket(config: &Spec, metadata: Option<&ConfigMetadata>) -> Option<PathBuf> {
    let field = metadata?.host_socket_field()?;
    let endpoint = config_str(config, field)?;
    omnifs_caps::endpoint_socket(endpoint)
        .ok()
        .flatten()
        .map(PathBuf::from)
}

fn dynamic_domains(config: &Spec, metadata: Option<&ConfigMetadata>) -> Vec<String> {
    let Some(field) = metadata.and_then(ConfigMetadata::domain_list_field) else {
        return Vec::new();
    };
    let Some(values) = config
        .config_raw
        .as_ref()
        .and_then(|config| config.get(field))
        .and_then(serde_json::Value::as_array)
    else {
        return Vec::new();
    };
    values
        .iter()
        .filter_map(serde_json::Value::as_str)
        .filter(|domain| *domain != "*")
        .map(ToString::to_string)
        .collect()
}

/// The string value of a mount config field, if present.
pub(crate) fn config_str<'a>(config: &'a Spec, field: &str) -> Option<&'a str> {
    config
        .config_raw
        .as_ref()
        .and_then(|config| config.get(field))
        .and_then(serde_json::Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::CapabilityChecker;
    use omnifs_wit::provider::types as wit_types;
    use omnifs_workspace::ids::{ProviderId, ProviderMeta, ProviderName, ProviderRef};
    use omnifs_workspace::mounts::Spec;

    #[test]
    fn provider_requested_unix_socket_is_not_granted() {
        let spec = Spec {
            provider: ProviderRef {
                id: ProviderId::from_wasm_bytes(b"test-provider"),
                meta: ProviderMeta {
                    name: ProviderName::new("test-provider").unwrap(),
                    version: None,
                },
            },
            mount: "test".to_owned(),
            root_mount: false,
            revalidate: true,
            auth: None,
            capabilities: None,
            limits: None,
            config_raw: None,
        };
        let mut provider_caps = wit_types::RequestedCapabilities::empty();
        provider_caps
            .unix_sockets
            .push("/tmp/provider.sock".to_owned());

        let checker = CapabilityChecker::from_config(&spec, &provider_caps, None);

        assert!(checker.grants().unix_sockets.is_empty());
    }
}
