//! The resolved runtime allowlist and the *decision* of whether a callout is
//! permitted. The host resolves a mount's [`Grants`](crate::Grants) into an
//! [`Allowlist`] (dynamic grants resolved to concrete values, runtime-requested
//! additions merged) and calls these checks before every provider callout.
//! This crate owns the decision; the host owns enforcement (acting on it).

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use url::Url;

use crate::matching::{domain_matches, glob_covers};

/// The concrete capabilities a mounted provider may use at runtime: the
/// allowlist the host enforces on every callout. Produced host-side from a
/// mount's grants.
#[derive(Debug, Clone, Default)]
pub struct Allowlist {
    pub domains: Vec<String>,
    pub git_repos: Vec<String>,
    pub max_memory_mb: u32,
    pub needs_git: bool,
    /// Absolute unix socket paths the provider may open via `unix:` URLs. Empty
    /// means no socket is allowed.
    pub unix_sockets: Vec<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
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

impl Allowlist {
    /// Validate a callout URL. `https://host/path?q` flows through the private-IP
    /// and domain-allowlist checks. `unix://<hex(socket_path)>/path?q` is a
    /// filesystem transport: it skips the HTTPS-and-IP rules (the socket is a
    /// filesystem object, not a network endpoint) and is gated only by the
    /// unix-socket allowlist. URL-internal path safety is the provider's
    /// responsibility, kept honest by the auditable provider source.
    pub fn check_url(&self, url: &str) -> Result<(), Error> {
        let parsed = Url::parse(url).map_err(|e| Error::InvalidUrl(e.to_string()))?;

        match parsed.scheme() {
            "https" => {
                let host = parsed
                    .host_str()
                    .ok_or_else(|| Error::InvalidUrl("no host".to_string()))?;

                // Check for private/link-local IPs (covers bare and bracketed IPv6).
                let bare_host = host.trim_start_matches('[').trim_end_matches(']');
                if let Ok(ip) = bare_host.parse::<IpAddr>()
                    && is_private_or_link_local(&ip)
                {
                    return Err(Error::PrivateIpDenied {
                        addr: ip.to_string(),
                    });
                }

                if !self.domain_allowed(host) {
                    return Err(Error::DomainDenied {
                        domain: host.to_string(),
                    });
                }
            },
            "unix" => {
                let socket_path = decode_unix_socket(&parsed)?;
                if !self.unix_socket_allowed(&socket_path) {
                    return Err(Error::UnixSocketDenied {
                        path: socket_path.display().to_string(),
                    });
                }
            },
            _ => return Err(Error::HttpDenied),
        }

        Ok(())
    }

    /// Decode the socket path from a `unix:` URL without allowlist checks. The
    /// executor uses this to open the socket once `check_url` has approved it.
    pub fn decode_unix_socket(url: &str) -> Result<PathBuf, Error> {
        let parsed = Url::parse(url).map_err(|e| Error::InvalidUrl(e.to_string()))?;
        if parsed.scheme() != "unix" {
            return Err(Error::InvalidUrl("expected unix:// scheme".to_string()));
        }
        decode_unix_socket(&parsed)
    }

    pub fn check_git_url(&self, url: &str) -> Result<(), Error> {
        if !self.needs_git {
            return Err(Error::GitNotGranted);
        }
        if !self.git_repo_allowed(url) {
            return Err(Error::GitRepoDenied {
                url: url.to_string(),
            });
        }
        Ok(())
    }

    fn domain_allowed(&self, host: &str) -> bool {
        self.domains
            .iter()
            .any(|allowed| domain_matches(allowed, host))
    }

    fn unix_socket_allowed(&self, socket_path: &Path) -> bool {
        self.unix_sockets
            .iter()
            .any(|allowed| allowed.as_path() == socket_path)
    }

    fn git_repo_allowed(&self, url: &str) -> bool {
        self.git_repos
            .iter()
            .any(|pattern| glob_covers(pattern, url))
    }
}

/// Decode the host component of a `unix:` URL to a socket path.
///
/// The host segment is the `hex` encoding of the absolute socket path bytes,
/// matching the `hyperlocal` convention so a hand-constructed URL interoperates
/// with other tooling. The decoded bytes must be valid UTF-8; non-UTF-8 socket
/// paths are rejected as `InvalidUrl`.
fn decode_unix_socket(parsed: &Url) -> Result<PathBuf, Error> {
    let host = parsed
        .host_str()
        .ok_or_else(|| Error::InvalidUrl("unix URL missing host segment".to_string()))?;
    let bytes = hex::decode(host)
        .map_err(|e| Error::InvalidUrl(format!("unix URL host is not hex-encoded: {e}")))?;
    let path_str = String::from_utf8(bytes)
        .map_err(|e| Error::InvalidUrl(format!("unix URL host decodes to non-UTF-8 path: {e}")))?;
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

    fn allow(domains: Vec<&str>, sockets: Vec<&str>) -> Allowlist {
        Allowlist {
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
        let allow = allow(vec!["api.example.com"], Vec::new());
        allow
            .check_url("https://api.example.com/v1/things")
            .expect("https GET against allowlisted domain must succeed");
    }

    #[test]
    fn https_callout_against_disallowed_domain_fails() {
        let allow = allow(vec!["api.example.com"], Vec::new());
        let err = allow.check_url("https://other.example.com/").unwrap_err();
        assert!(matches!(err, Error::DomainDenied { .. }));
    }

    #[test]
    fn http_scheme_is_denied() {
        let allow = allow(vec!["api.example.com"], Vec::new());
        let err = allow.check_url("http://api.example.com/").unwrap_err();
        assert!(matches!(err, Error::HttpDenied));
    }

    #[test]
    fn private_ip_is_denied() {
        let allow = allow(vec!["10.0.0.1"], Vec::new());
        let err = allow.check_url("https://10.0.0.1/v1/things").unwrap_err();
        assert!(matches!(err, Error::PrivateIpDenied { .. }));
    }

    #[test]
    fn unix_callout_against_allowed_socket_passes() {
        let allow = allow(Vec::new(), vec!["/var/run/docker.sock"]);
        let url = unix_url("/var/run/docker.sock", "/v1.43/containers/json");
        allow
            .check_url(&url)
            .expect("unix GET against allowlisted socket must succeed");
    }

    #[test]
    fn unix_callout_against_disallowed_socket_fails() {
        let allow = allow(Vec::new(), vec!["/var/run/docker.sock"]);
        let url = unix_url("/var/run/other.sock", "/v1.43/containers/json");
        let err = allow.check_url(&url).unwrap_err();
        assert!(matches!(err, Error::UnixSocketDenied { .. }));
    }

    #[test]
    fn unix_url_skips_https_and_private_ip_rules() {
        // The socket path looks IP-ish and would trip the private-IP check if
        // we still applied that rule to unix URLs. We don't.
        let allow = allow(Vec::new(), vec!["/127.0.0.1"]);
        let url = unix_url("/127.0.0.1", "/whatever");
        allow.check_url(&url).unwrap();
    }

    #[test]
    fn git_glob_grant_allows_repos_under_the_prefix() {
        let allow = Allowlist {
            git_repos: vec!["git@github.com:*".into()],
            needs_git: true,
            ..Allowlist::default()
        };
        allow.check_git_url("git@github.com:me/repo").unwrap();
        assert!(matches!(
            allow.check_git_url("git@gitlab.com:me/repo"),
            Err(Error::GitRepoDenied { .. })
        ));
    }

    #[test]
    fn decode_unix_socket_rejects_non_unix_scheme() {
        let err = Allowlist::decode_unix_socket("https://example.com/").unwrap_err();
        assert!(matches!(err, Error::InvalidUrl(_)));
    }
}
