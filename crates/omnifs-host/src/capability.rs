//! Capability checking for provider sandboxing.
//!
//! Validates HTTP domains, IP addresses, request methods, request path
//! prefixes, unix socket paths, and Git repository URLs against
//! provider capability grants.

use std::net::IpAddr;
use std::path::PathBuf;

use omnifs_mount_schema::mounts::Resolved;
use omnifs_wit::provider::types as wit_types;
use url::Url;

#[derive(Debug, Clone, Default)]
pub struct CapabilityGrants {
    pub domains: Vec<String>,
    pub git_repos: Vec<String>,
    pub max_memory_mb: u32,
    pub needs_git: bool,
    /// Absolute unix socket paths the provider may open via `unix:`
    /// URLs. An empty list means no socket is allowed.
    pub unix_sockets: Vec<PathBuf>,
}

impl CapabilityGrants {
    pub fn from_config(
        config: &Resolved,
        provider_caps: &wit_types::RequestedCapabilities,
    ) -> Self {
        let caps = config.capabilities.as_ref();
        let mut unix_sockets: Vec<PathBuf> = caps
            .and_then(|c| c.unix_sockets.clone())
            .unwrap_or_default()
            .into_iter()
            .map(PathBuf::from)
            .collect();
        unix_sockets.extend(provider_caps.unix_sockets.iter().map(PathBuf::from));
        unix_sockets.sort();
        unix_sockets.dedup();

        Self {
            domains: caps.and_then(|c| c.domains.clone()).unwrap_or_default(),
            git_repos: caps.and_then(|c| c.git_repos.clone()).unwrap_or_default(),
            max_memory_mb: caps.and_then(|c| c.max_memory_mb).unwrap_or(64),
            needs_git: provider_caps.needs_git,
            unix_sockets,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CapabilityError {
    #[error("domain not in allowlist: {domain}")]
    DomainDenied { domain: String },
    #[error("HTTP not allowed (HTTPS or unix: required)")]
    HttpDenied,
    #[error("private/link-local IP target denied: {addr}")]
    PrivateIpDenied { addr: String },
    #[error("git capability not granted")]
    GitNotGranted,
    #[error("git repo not in allowlist: {url}")]
    GitRepoDenied { url: String },
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    #[error("unix socket not in allowlist: {path}")]
    UnixSocketDenied { path: String },
}

pub struct CapabilityChecker {
    grants: CapabilityGrants,
}

impl CapabilityChecker {
    pub fn new(grants: CapabilityGrants) -> Self {
        Self { grants }
    }

    pub fn grants(&self) -> &CapabilityGrants {
        &self.grants
    }

    /// Validate a callout URL. `https://host/path?q` flows through
    /// the existing checks (private IPs, domain allowlist).
    /// `unix://<hex(socket_path)>/path?q` is a new transport: it
    /// skips the HTTPS-and-IP rules (the socket is a filesystem
    /// object, not a network endpoint) and is gated only by the
    /// unix-socket allowlist. URL-internal path safety (e.g. only
    /// hitting a closed set of daemon endpoints) is the provider's
    /// responsibility, kept honest by the fact that the provider
    /// source is auditable.
    pub fn check_url(&self, url: &str) -> Result<(), CapabilityError> {
        let parsed = Url::parse(url).map_err(|e| CapabilityError::InvalidUrl(e.to_string()))?;

        match parsed.scheme() {
            "https" => {
                let host = parsed
                    .host_str()
                    .ok_or_else(|| CapabilityError::InvalidUrl("no host".to_string()))?;

                // Check for private/link-local IPs (covers both bare and bracketed IPv6).
                let bare_host = host.trim_start_matches('[').trim_end_matches(']');
                if let Ok(ip) = bare_host.parse::<IpAddr>()
                    && is_private_or_link_local(&ip)
                {
                    return Err(CapabilityError::PrivateIpDenied {
                        addr: ip.to_string(),
                    });
                }

                if !self.domain_allowed(host) {
                    return Err(CapabilityError::DomainDenied {
                        domain: host.to_string(),
                    });
                }
            },
            "unix" => {
                let socket_path = decode_unix_socket(&parsed)?;
                if !self.unix_socket_allowed(&socket_path) {
                    return Err(CapabilityError::UnixSocketDenied {
                        path: socket_path.display().to_string(),
                    });
                }
            },
            _ => return Err(CapabilityError::HttpDenied),
        }

        Ok(())
    }

    /// Decode the socket path from a `unix:` URL without performing
    /// allowlist checks. Used by the executor to actually open the
    /// socket once `check_url` has approved the request.
    pub fn decode_unix_socket(url: &str) -> Result<PathBuf, CapabilityError> {
        let parsed = Url::parse(url).map_err(|e| CapabilityError::InvalidUrl(e.to_string()))?;
        if parsed.scheme() != "unix" {
            return Err(CapabilityError::InvalidUrl(
                "expected unix:// scheme".to_string(),
            ));
        }
        decode_unix_socket(&parsed)
    }

    pub fn check_git_url(&self, url: &str) -> Result<(), CapabilityError> {
        if !self.grants.needs_git {
            return Err(CapabilityError::GitNotGranted);
        }
        if !self.git_repo_allowed(url) {
            return Err(CapabilityError::GitRepoDenied {
                url: url.to_string(),
            });
        }
        Ok(())
    }

    fn domain_allowed(&self, host: &str) -> bool {
        self.grants
            .domains
            .iter()
            .any(|allowed| allowed == "*" || host == allowed)
    }

    fn unix_socket_allowed(&self, socket_path: &std::path::Path) -> bool {
        self.grants
            .unix_sockets
            .iter()
            .any(|allowed| allowed.as_path() == socket_path)
    }

    fn git_repo_allowed(&self, url: &str) -> bool {
        self.grants.git_repos.iter().any(|pattern| {
            if let Some(prefix) = pattern.strip_suffix('*') {
                url.starts_with(prefix)
            } else {
                url == pattern
            }
        })
    }
}

/// Decode the host component of a `unix:` URL to a socket path.
///
/// We use `hex` encoding of the absolute socket path bytes as the
/// host segment, matching the `hyperlocal` convention so a
/// hand-constructed URL is interoperable with other tooling. The
/// host bytes must be valid UTF-8 once decoded; non-UTF-8 socket
/// paths are rejected as `InvalidUrl`.
fn decode_unix_socket(parsed: &Url) -> Result<PathBuf, CapabilityError> {
    let host = parsed
        .host_str()
        .ok_or_else(|| CapabilityError::InvalidUrl("unix URL missing host segment".to_string()))?;
    let bytes = hex::decode(host).map_err(|e| {
        CapabilityError::InvalidUrl(format!("unix URL host is not hex-encoded: {e}"))
    })?;
    let path_str = String::from_utf8(bytes).map_err(|e| {
        CapabilityError::InvalidUrl(format!("unix URL host decodes to non-UTF-8 path: {e}"))
    })?;
    Ok(PathBuf::from(path_str))
}

fn is_private_or_link_local(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || (v4.octets()[0] == 169 && v4.octets()[1] == 254)
        },
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || {
                    // Link-local: fe80::/10
                    let segments = v6.segments();
                    (segments[0] & 0xffc0) == 0xfe80
                }
                || {
                    // Unique local: fc00::/7
                    let segments = v6.segments();
                    (segments[0] & 0xfe00) == 0xfc00
                }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grants(domains: Vec<&str>, sockets: Vec<&str>) -> CapabilityGrants {
        CapabilityGrants {
            domains: domains.into_iter().map(String::from).collect(),
            git_repos: Vec::new(),
            max_memory_mb: 64,
            needs_git: false,
            unix_sockets: sockets.into_iter().map(PathBuf::from).collect(),
        }
    }

    fn unix_url(socket: &str, path: &str) -> String {
        format!("unix://{}{path}", hex::encode(socket))
    }

    #[test]
    fn https_callout_against_allowed_domain_passes() {
        let checker = CapabilityChecker::new(grants(vec!["api.example.com"], Vec::new()));
        checker
            .check_url("https://api.example.com/v1/things")
            .expect("https GET against allowlisted domain must succeed");
    }

    #[test]
    fn https_callout_against_disallowed_domain_fails() {
        let checker = CapabilityChecker::new(grants(vec!["api.example.com"], Vec::new()));
        let err = checker.check_url("https://other.example.com/").unwrap_err();
        assert!(matches!(err, CapabilityError::DomainDenied { .. }));
    }

    #[test]
    fn http_scheme_is_denied() {
        let checker = CapabilityChecker::new(grants(vec!["api.example.com"], Vec::new()));
        let err = checker.check_url("http://api.example.com/").unwrap_err();
        assert!(matches!(err, CapabilityError::HttpDenied));
    }

    #[test]
    fn private_ip_is_denied() {
        let checker = CapabilityChecker::new(grants(vec!["10.0.0.1"], Vec::new()));
        let err = checker.check_url("https://10.0.0.1/v1/things").unwrap_err();
        assert!(matches!(err, CapabilityError::PrivateIpDenied { .. }));
    }

    #[test]
    fn unix_callout_against_allowed_socket_passes() {
        let checker = CapabilityChecker::new(grants(Vec::new(), vec!["/var/run/docker.sock"]));
        let url = unix_url("/var/run/docker.sock", "/v1.43/containers/json");
        checker
            .check_url(&url)
            .expect("unix GET against allowlisted socket must succeed");
    }

    #[test]
    fn unix_callout_against_disallowed_socket_fails() {
        let checker = CapabilityChecker::new(grants(Vec::new(), vec!["/var/run/docker.sock"]));
        let url = unix_url("/var/run/other.sock", "/v1.43/containers/json");
        let err = checker.check_url(&url).unwrap_err();
        assert!(matches!(err, CapabilityError::UnixSocketDenied { .. }));
    }

    #[test]
    fn unix_url_skips_https_and_private_ip_rules() {
        // The unix socket path looks IP-ish and would trip the private
        // IP check if we still applied that rule. We don't.
        let checker = CapabilityChecker::new(grants(Vec::new(), vec!["/127.0.0.1"]));
        let url = unix_url("/127.0.0.1", "/whatever");
        checker.check_url(&url).unwrap();
    }

    #[test]
    fn decode_unix_socket_rejects_non_unix_scheme() {
        let err = CapabilityChecker::decode_unix_socket("https://example.com/").unwrap_err();
        assert!(matches!(err, CapabilityError::InvalidUrl(_)));
    }
}
