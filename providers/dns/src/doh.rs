use std::collections::BTreeMap;
use std::net::IpAddr;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hickory_proto::op::{Message, MessageType, OpCode, Query as DnsQuery, ResponseCode};
use hickory_proto::rr::Name;

use crate::types::{ResolverName, SupportedRecordType};
use omnifs_sdk::prelude::*;
use omnifs_sdk::serde::Deserialize;

const BUILTIN_DEFAULTS_JSON: &str = r#"{
  "default_resolver": "cloudflare",
  "resolvers": {
    "cloudflare": {
      "url": "https://cloudflare-dns.com/dns-query",
      "aliases": ["1.1.1.1", "1.0.0.1"]
    },
    "google": {
      "url": "https://dns.google/dns-query",
      "aliases": ["8.8.8.8", "8.8.4.4", "dns.google"]
    }
  }
}"#;

#[derive(Deserialize)]
#[serde(default)]
struct RawConfig {
    default_resolver: String,
    #[serde(default)]
    resolvers: BTreeMap<String, RawResolver>,
}

impl Default for RawConfig {
    fn default() -> Self {
        Self {
            default_resolver: "cloudflare".to_string(),
            resolvers: BTreeMap::new(),
        }
    }
}

#[derive(Deserialize)]
struct RawResolver {
    url: String,
    #[serde(default)]
    aliases: Vec<String>,
}

fn parse_raw_resolvers(bytes: &[u8]) -> Result<RawConfig> {
    omnifs_sdk::serde_json::from_slice(bytes)
        .map_err(|error| ProviderError::invalid_input(format!("invalid resolver config: {error}")))
}

fn build_resolver_entries(
    raw_resolvers: BTreeMap<String, RawResolver>,
) -> Result<Vec<ResolverEntry>> {
    raw_resolvers
        .into_iter()
        .map(|(name, raw)| {
            name.parse::<ResolverName>().map_err(|()| {
                ProviderError::invalid_input(format!("invalid resolver name: {name}"))
            })?;
            let url = Endpoint::new(raw.url).map_err(|error| {
                ProviderError::invalid_input(format!("invalid resolver {name:?}: {error}"))
            })?;
            Ok(ResolverEntry {
                name,
                url,
                aliases: raw.aliases,
            })
        })
        .collect()
}

/// Validated `DoH` endpoint URL (always HTTPS).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Endpoint(String);

impl Endpoint {
    fn new(url: impl Into<String>) -> std::result::Result<Self, String> {
        let url = url.into();
        if !url.starts_with("https://") {
            return Err(format!("DoH endpoint must use HTTPS: {url}"));
        }
        Ok(Self(url))
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl PartialEq<&str> for Endpoint {
    fn eq(&self, other: &&str) -> bool {
        self.0.as_str() == *other
    }
}

/// Resolver aliases and their `DoH` endpoints, parsed from provider config.
///
/// Example JSON (as received by the provider, `config` object only):
/// ```json
/// {
///   "default_resolver": "cloudflare",
///   "resolvers": {
///     "cloudflare": {
///       "url": "https://cloudflare-dns.com/dns-query",
///       "aliases": ["1.1.1.1", "1.0.0.1"]
///     }
///   }
/// }
/// ```
#[derive(Debug, Clone)]
pub(super) struct ResolverConfig {
    default_name: String,
    resolvers: Vec<ResolverEntry>,
}

#[derive(Debug, Clone)]
struct ResolverEntry {
    name: String,
    url: Endpoint,
    aliases: Vec<String>,
}

impl ResolverConfig {
    /// Build from already-deserialized config maps (called from `init`).
    pub(super) fn from_config<I>(default_resolver: String, raw_resolvers: I) -> Result<Self>
    where
        I: IntoIterator<Item = (String, crate::ConfigResolver)>,
    {
        let resolvers: BTreeMap<_, _> = raw_resolvers
            .into_iter()
            .map(|(name, resolver)| {
                (
                    name,
                    RawResolver {
                        url: resolver.url,
                        aliases: resolver.aliases,
                    },
                )
            })
            .collect();

        let resolvers = if resolvers.is_empty() {
            Self::builtin_defaults()?
        } else {
            build_resolver_entries(resolvers)?
        };

        let config = Self {
            default_name: default_resolver,
            resolvers,
        };
        let _ = config.default_endpoint()?;
        Ok(config)
    }

    fn builtin_defaults() -> Result<Vec<ResolverEntry>> {
        let raw = parse_raw_resolvers(BUILTIN_DEFAULTS_JSON.as_bytes())?;
        build_resolver_entries(raw.resolvers)
    }

    fn resolve_endpoint(&self, specifier: Option<&str>) -> Result<Endpoint> {
        let Some(spec) = specifier else {
            return self.default_endpoint();
        };

        if spec.contains("://") {
            return Endpoint::new(spec).map_err(ProviderError::invalid_input);
        }

        self.lookup(spec).ok_or_else(|| {
            ProviderError::invalid_input(format!("unknown resolver specifier: {spec}"))
        })
    }

    fn lookup(&self, spec: &str) -> Option<Endpoint> {
        self.resolvers
            .iter()
            .find(|e| e.name == spec || e.aliases.iter().any(|a| a == spec))
            .map(|e| e.url.clone())
    }

    fn default_endpoint(&self) -> Result<Endpoint> {
        self.lookup(&self.default_name).ok_or_else(|| {
            ProviderError::invalid_input(format!(
                "default resolver {default:?} is not configured",
                default = self.default_name
            ))
        })
    }

    /// Format `_resolvers` file content from configured resolvers.
    pub(super) fn format_resolvers_file(&self) -> String {
        self.resolvers
            .iter()
            .map(|e| format!("{}\t{}\t{}", e.name, e.aliases.join(","), e.url))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    }

    pub(super) fn resolver_names(&self) -> Vec<String> {
        self.resolvers.iter().map(|e| e.name.clone()).collect()
    }

    /// Build from raw JSON bytes (used by tests only).
    #[cfg(test)]
    fn from_json(config_bytes: &[u8]) -> Result<Self> {
        let raw = parse_raw_resolvers(config_bytes)?;
        let resolvers = if raw.resolvers.is_empty() {
            Self::builtin_defaults()?
        } else {
            build_resolver_entries(raw.resolvers)?
        };

        let config = Self {
            default_name: raw.default_resolver,
            resolvers,
        };
        let _ = config.default_endpoint()?;
        Ok(config)
    }
}

/// Build a `DoH` query URL for the new async SDK (returns URL string).
pub(super) fn query_url(
    config: &ResolverConfig,
    resolver: Option<&str>,
    domain: &str,
    rtype: SupportedRecordType,
) -> Result<String> {
    let name = parse_name(domain)?;
    query_with_name(config, resolver, name, rtype)
}

pub(super) fn parse_response(body: &[u8]) -> Result<(Vec<crate::DnsRecord>, u64)> {
    const DEFAULT_TTL_SECS: u64 = 300;

    let response = Message::from_vec(body).map_err(|error| {
        ProviderError::invalid_input(format!("invalid DoH DNS message: {error}"))
    })?;

    if response.response_code != ResponseCode::NoError {
        let message = format!("DNS response code: {}", response.response_code);
        return Err(match response.response_code {
            ResponseCode::FormErr => ProviderError::invalid_input(message),
            ResponseCode::ServFail => ProviderError::network(message),
            ResponseCode::NXDomain => ProviderError::not_found(message),
            ResponseCode::Refused => ProviderError::denied(message),
            _ => ProviderError::internal(message),
        });
    }

    let mut min_ttl = None;
    let mut records = Vec::new();

    for answer in &response.answers {
        let Some(rtype) = SupportedRecordType::from_hickory(answer.record_type()) else {
            continue;
        };
        let ttl = u64::from(answer.ttl);
        min_ttl = Some(min_ttl.unwrap_or(ttl).min(ttl));
        records.push(crate::DnsRecord {
            rtype,
            value: answer.data.to_string(),
        });
    }

    Ok((records, min_ttl.unwrap_or(DEFAULT_TTL_SECS)))
}

/// Build a reverse `DNS` query URL for the new async SDK (returns URL string).
pub(super) fn reverse_query_url(
    config: &ResolverConfig,
    resolver: Option<&str>,
    ip: &str,
) -> Result<String> {
    let addr = ip
        .parse::<IpAddr>()
        .map_err(|_| ProviderError::invalid_input(format!("invalid IP address: {ip}")))?;
    let name = Name::from(addr);
    query_with_name(config, resolver, name, SupportedRecordType::PTR)
}

fn parse_name(domain: &str) -> Result<Name> {
    let fqdn = if domain.ends_with('.') {
        domain.to_string()
    } else {
        format!("{domain}.")
    };
    Name::from_ascii(&fqdn).map_err(|error| {
        ProviderError::invalid_input(format!("invalid domain name {domain:?}: {error}"))
    })
}

fn query_with_name(
    config: &ResolverConfig,
    resolver: Option<&str>,
    name: Name,
    rtype: SupportedRecordType,
) -> Result<String> {
    let endpoint = config.resolve_endpoint(resolver)?;
    let ep = endpoint.as_str();
    let sep = if ep.contains('?') { '&' } else { '?' };
    let dns_query = encode_dns_query(&name, rtype)?;
    Ok(format!("{ep}{sep}dns={dns_query}"))
}

fn encode_dns_query(name: &Name, rtype: SupportedRecordType) -> Result<String> {
    let mut message = Message::new(0, MessageType::Query, OpCode::Query);
    message.add_query(DnsQuery::query(name.clone(), rtype.as_hickory()));
    message.metadata.recursion_desired = true;

    let wire = message
        .to_vec()
        .map_err(|error| ProviderError::internal(format!("failed to encode DNS query: {error}")))?;

    Ok(URL_SAFE_NO_PAD.encode(wire))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use hickory_proto::rr::RecordType as HickoryRecordType;
    use omnifs_sdk::error::ProviderErrorKind;

    fn default_config() -> ResolverConfig {
        ResolverConfig::from_json(b"{}").unwrap()
    }

    #[test]
    fn unknown_resolver_specifier_is_rejected() {
        let cfg = default_config();
        let err = cfg.resolve_endpoint(Some("unknown")).unwrap_err();
        assert_eq!(err.kind(), ProviderErrorKind::InvalidInput);
        assert!(err.to_string().contains("unknown resolver specifier"));
    }
    #[test]
    fn query_uses_dns_wireformat_parameter() {
        let cfg = default_config();
        let url = query_url(&cfg, None, "ibm.com", SupportedRecordType::A).expect("query url");

        let (_, dns_param) = url
            .split_once("dns=")
            .expect("expected dns query parameter");
        let wire = URL_SAFE_NO_PAD.decode(dns_param).unwrap();
        let message = Message::from_vec(&wire).unwrap();

        assert!(message.metadata.recursion_desired);
        assert_eq!(message.queries.len(), 1);
        assert_eq!(message.queries[0].name.to_string(), "ibm.com.");
        assert_eq!(message.queries[0].query_type, HickoryRecordType::A);
    }

    #[test]
    fn reverse_query_uses_hickory_ptr_name() {
        let cfg = default_config();
        let url = reverse_query_url(&cfg, None, "26.3.0.103").expect("reverse query url");
        let (_, dns_param) = url.split_once("dns=").expect("dns param");
        let message = Message::from_vec(&URL_SAFE_NO_PAD.decode(dns_param).unwrap()).unwrap();
        assert_eq!(message.queries[0].query_type, HickoryRecordType::PTR);
        assert_eq!(
            message.queries[0].name.to_string(),
            "103.0.3.26.in-addr.arpa."
        );
    }
}
