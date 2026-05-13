//! Capability checking for provider sandboxing.
//!
//! Validates HTTP domains, IP addresses, and Git repository URLs
//! against provider capability grants.

use std::net::IpAddr;

use url::Url;

#[derive(Debug, Clone)]
pub struct CapabilityGrants {
    pub domains: Vec<String>,
    pub git_repos: Vec<String>,
    pub max_memory_mb: u32,
    pub needs_git: bool,
}

impl CapabilityGrants {
    pub fn from_config(config: &crate::config::InstanceConfig, needs_git: bool) -> Self {
        let caps = config.capabilities.as_ref();
        Self {
            domains: caps.and_then(|c| c.domains.clone()).unwrap_or_default(),
            git_repos: caps.and_then(|c| c.git_repos.clone()).unwrap_or_default(),
            max_memory_mb: caps.and_then(|c| c.max_memory_mb).unwrap_or(64),
            needs_git,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CapabilityError {
    #[error("domain not in allowlist: {domain}")]
    DomainDenied { domain: String },
    #[error("HTTP not allowed (HTTPS required)")]
    HttpDenied,
    #[error("private/link-local IP target denied: {addr}")]
    PrivateIpDenied { addr: String },
    #[error("git capability not granted")]
    GitNotGranted,
    #[error("git repo not in allowlist: {url}")]
    GitRepoDenied { url: String },
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
}

pub struct CapabilityChecker {
    grants: CapabilityGrants,
}

impl CapabilityChecker {
    pub fn new(grants: CapabilityGrants) -> Self {
        Self { grants }
    }

    pub fn check_url(&self, url: &str) -> Result<(), CapabilityError> {
        let parsed = Url::parse(url).map_err(|e| CapabilityError::InvalidUrl(e.to_string()))?;

        if parsed.scheme() != "https" {
            return Err(CapabilityError::HttpDenied);
        }

        let host = parsed
            .host_str()
            .ok_or_else(|| CapabilityError::InvalidUrl("no host".to_string()))?;

        // Check for private/link-local IPs (covers both bare and bracketed IPv6)
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

        Ok(())
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
