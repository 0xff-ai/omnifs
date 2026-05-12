use std::collections::BTreeMap;
use std::fmt::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hickory_proto::op::{Message, MessageType, OpCode, Query as DnsQuery, ResponseCode};
use hickory_proto::rr::{Name, RecordType as HickoryRecordType};

use crate::types::{RecordType, ResolverName};
#[cfg(test)]
use omnifs_sdk::omnifs::provider::types::{Callout, Header, HttpRequest};
use omnifs_sdk::prelude::*;
use omnifs_sdk::serde::Deserialize;

#[cfg(test)]
const CLOUDFLARE_DOH: &str = "https://cloudflare-dns.com/dns-query";
#[cfg(test)]
const GOOGLE_DOH: &str = "https://dns.google/dns-query";

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

#[derive(Debug)]
enum DohError {
    Parse(String),
    DnsResponse(ResponseCode),
}

impl DohError {
    fn into_provider_error(self) -> ProviderError {
        match self {
            Self::Parse(message) => {
                ProviderError::invalid_input(format!("invalid DoH DNS message: {message}"))
            },
            Self::DnsResponse(code) => {
                let message = format!("DNS response code: {code}");
                match code {
                    ResponseCode::FormErr => ProviderError::invalid_input(message),
                    ResponseCode::ServFail => ProviderError::network(message),
                    ResponseCode::NXDomain => ProviderError::not_found(message),
                    ResponseCode::Refused => ProviderError::denied(message),
                    _ => ProviderError::internal(message),
                }
            },
        }
    }
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
    rtype: RecordType,
) -> Result<String> {
    let endpoint = config.resolve_endpoint(resolver)?;
    let ep = endpoint.as_str();
    let sep = if ep.contains('?') { '&' } else { '?' };
    let dns_query = encode_dns_query(domain, rtype)?;
    let url = format!("{ep}{sep}dns={dns_query}");
    Ok(url)
}

/// Callout-based query function (kept for potential test usage).
#[cfg(test)]
fn query(
    config: &ResolverConfig,
    resolver: Option<&str>,
    domain: &str,
    rtype: RecordType,
) -> Result<Callout> {
    let url = query_url(config, resolver, domain, rtype)?;
    Ok(Callout::Fetch(HttpRequest {
        method: "GET".to_string(),
        url,
        headers: vec![Header {
            name: "Accept".to_string(),
            value: "application/dns-message".to_string(),
        }],
        body: None,
    }))
}

pub(super) fn parse_response(body: &[u8]) -> Result<(Vec<crate::DnsRecord>, u64)> {
    parse_doh_response(body).map_err(DohError::into_provider_error)
}

fn parse_doh_response(body: &[u8]) -> std::result::Result<(Vec<crate::DnsRecord>, u64), DohError> {
    let response = Message::from_vec(body).map_err(|e| DohError::Parse(e.to_string()))?;

    if response.response_code != ResponseCode::NoError {
        return Err(DohError::DnsResponse(response.response_code));
    }

    let mut min_ttl = u64::MAX;
    let mut records = Vec::new();

    for answer in &response.answers {
        let maybe_type = RecordType::from_wire(u16::from(answer.record_type()));
        if let Some(rtype) = maybe_type {
            min_ttl = min_ttl.min(u64::from(answer.ttl));
            records.push(crate::DnsRecord {
                rtype,
                value: answer.data.to_string(),
            });
        }
    }

    Ok((records, if min_ttl == u64::MAX { 300 } else { min_ttl }))
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
    let ptr_domain = match addr {
        IpAddr::V4(addr) => ip_to_in_addr_arpa(addr),
        IpAddr::V6(addr) => ip_to_ip6_arpa(addr),
    };
    query_url(config, resolver, &ptr_domain, RecordType::PTR)
}

/// Callout-based reverse query function (kept for potential test usage).
#[cfg(test)]
fn reverse_query(config: &ResolverConfig, resolver: Option<&str>, ip: &str) -> Result<Callout> {
    let addr = ip
        .parse::<IpAddr>()
        .map_err(|_| ProviderError::invalid_input(format!("invalid IP address: {ip}")))?;
    let ptr_domain = match addr {
        IpAddr::V4(addr) => ip_to_in_addr_arpa(addr),
        IpAddr::V6(addr) => ip_to_ip6_arpa(addr),
    };
    query(config, resolver, &ptr_domain, RecordType::PTR)
}

fn encode_dns_query(domain: &str, rtype: RecordType) -> Result<String> {
    let mut message = Message::new(0, MessageType::Query, OpCode::Query);
    let mut fqdn = domain.to_string();
    if !fqdn.ends_with('.') {
        fqdn.push('.');
    }

    let name = Name::from_ascii(&fqdn).map_err(|error| {
        ProviderError::invalid_input(format!("invalid domain name {domain:?}: {error}"))
    })?;

    let hickory_rtype = match rtype {
        RecordType::A => HickoryRecordType::A,
        RecordType::AAAA => HickoryRecordType::AAAA,
        RecordType::CNAME => HickoryRecordType::CNAME,
        RecordType::MX => HickoryRecordType::MX,
        RecordType::NS => HickoryRecordType::NS,
        RecordType::TXT => HickoryRecordType::TXT,
        RecordType::SOA => HickoryRecordType::SOA,
        RecordType::SRV => HickoryRecordType::SRV,
        RecordType::CAA => HickoryRecordType::CAA,
        RecordType::PTR => HickoryRecordType::PTR,
    };

    message.add_query(DnsQuery::query(name, hickory_rtype));
    message.metadata.recursion_desired = true;

    let wire = message
        .to_vec()
        .map_err(|error| ProviderError::internal(format!("failed to encode DNS query: {error}")))?;

    Ok(URL_SAFE_NO_PAD.encode(wire))
}

fn ip_to_in_addr_arpa(ip: Ipv4Addr) -> String {
    let octets = ip.octets();
    format!(
        "{}.{}.{}.{}.in-addr.arpa",
        octets[3], octets[2], octets[1], octets[0]
    )
}

fn ip_to_ip6_arpa(ip: Ipv6Addr) -> String {
    // 32 nibbles + 31 dots + ".ip6.arpa" (9) = 72
    let mut out = String::with_capacity(72);
    for (i, nibble) in ip
        .octets()
        .iter()
        .rev()
        .flat_map(|byte| [byte & 0x0f, byte >> 4])
        .enumerate()
    {
        if i > 0 {
            out.push('.');
        }
        let _ = write!(out, "{nibble:x}");
    }
    out.push_str(".ip6.arpa");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use hickory_proto::rr::{RData, Record, rdata::A};
    use omnifs_sdk::error::ProviderErrorKind;

    fn default_config() -> ResolverConfig {
        ResolverConfig::from_json(b"{}").unwrap()
    }

    fn make_dns_message_response(
        response_code: ResponseCode,
        answers: impl IntoIterator<Item = (&'static str, u32, &'static str)>,
    ) -> Vec<u8> {
        let mut response = Message::new(0, MessageType::Response, OpCode::Query);
        response.metadata.response_code = response_code;
        let records: Vec<_> = answers
            .into_iter()
            .map(|(domain, ttl, address)| make_a_record(domain, ttl, address))
            .collect();
        response.add_answers(records);
        response.to_vec().unwrap()
    }

    fn make_a_record(domain: &str, ttl: u32, address: &str) -> Record {
        Record::from_rdata(
            Name::from_ascii(domain).unwrap(),
            ttl,
            RData::A(A(address.parse::<Ipv4Addr>().unwrap())),
        )
    }

    #[test]
    fn default_resolves_to_cloudflare() {
        let cfg = default_config();
        assert_eq!(cfg.resolve_endpoint(None).unwrap(), CLOUDFLARE_DOH);
        assert_eq!(
            cfg.resolve_endpoint(Some("cloudflare")).unwrap(),
            CLOUDFLARE_DOH
        );
        assert_eq!(
            cfg.resolve_endpoint(Some("1.1.1.1")).unwrap(),
            CLOUDFLARE_DOH
        );
    }

    #[test]
    fn alias_resolves_google() {
        let cfg = default_config();
        assert_eq!(cfg.resolve_endpoint(Some("google")).unwrap(), GOOGLE_DOH);
        assert_eq!(cfg.resolve_endpoint(Some("8.8.8.8")).unwrap(), GOOGLE_DOH);
        assert_eq!(
            cfg.resolve_endpoint(Some("dns.google")).unwrap(),
            GOOGLE_DOH
        );
    }

    #[test]
    fn custom_resolver_from_config() {
        let json = br#"{
            "default_resolver": "quad9",
            "resolvers": {
                "quad9": {
                    "url": "https://dns.quad9.net:5053/dns-query",
                    "aliases": ["9.9.9.9"]
                }
            }
        }"#;
        let cfg = ResolverConfig::from_json(json).unwrap();
        assert_eq!(
            cfg.resolve_endpoint(None).unwrap(),
            "https://dns.quad9.net:5053/dns-query"
        );
        assert_eq!(
            cfg.resolve_endpoint(Some("9.9.9.9")).unwrap(),
            "https://dns.quad9.net:5053/dns-query"
        );
    }

    #[test]
    fn https_url_passthrough() {
        let cfg = default_config();
        assert_eq!(
            cfg.resolve_endpoint(Some("https://custom.dns/query"))
                .unwrap(),
            "https://custom.dns/query"
        );
    }

    #[test]
    fn unknown_resolver_specifier_is_rejected() {
        let cfg = default_config();
        let err = cfg.resolve_endpoint(Some("unknown")).unwrap_err();
        assert_eq!(err.kind(), ProviderErrorKind::InvalidInput);
        assert!(err.to_string().contains("unknown resolver specifier"));
    }

    #[test]
    fn missing_default_resolver_is_rejected() {
        let json = br#"{
            "default_resolver": "quad9",
            "resolvers": {
                "cloudflare": {
                    "url": "https://cloudflare-dns.com/dns-query",
                    "aliases": ["1.1.1.1"]
                }
            }
        }"#;
        let err = ResolverConfig::from_json(json).unwrap_err();
        assert_eq!(err.kind(), ProviderErrorKind::InvalidInput);
        assert!(err.to_string().contains("default resolver"));
    }

    #[test]
    fn invalid_json_config_is_rejected() {
        let err = ResolverConfig::from_json(b"{").unwrap_err();
        assert_eq!(err.kind(), ProviderErrorKind::InvalidInput);
        assert!(err.to_string().contains("invalid resolver config"));
    }

    #[test]
    fn in_addr_arpa() {
        assert_eq!(
            ip_to_in_addr_arpa(Ipv4Addr::new(93, 184, 216, 34)),
            "34.216.184.93.in-addr.arpa"
        );
    }

    #[test]
    fn ip6_arpa() {
        assert_eq!(
            ip_to_ip6_arpa(Ipv6Addr::LOCALHOST),
            "1.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.ip6.arpa"
        );
    }

    #[test]
    fn invalid_reverse_ip_is_rejected() {
        let cfg = default_config();
        assert!(matches!(
            reverse_query(&cfg, None, "1:2:3:4:5:6:7:8:9"),
            Err(err) if format!("{err}").contains("invalid IP address")
        ));
        assert!(matches!(
            reverse_query(&cfg, None, "999.184.216.34"),
            Err(err) if format!("{err}").contains("invalid IP address")
        ));
    }

    #[test]
    fn parse_doh_response() {
        let response = make_dns_message_response(
            ResponseCode::NoError,
            [
                ("example.com", 300, "93.184.216.34"),
                ("example.com", 200, "93.184.216.35"),
            ],
        );
        let (records, ttl) = parse_response(&response).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].value, "93.184.216.34");
        assert_eq!(records[1].value, "93.184.216.35");
        assert_eq!(ttl, 200);
    }

    #[test]
    fn query_uses_dns_wireformat_parameter() {
        let cfg = default_config();
        let Ok(Callout::Fetch(request)) = query(&cfg, None, "ibm.com", RecordType::A) else {
            panic!("expected fetch callout");
        };

        assert_eq!(request.method, "GET");
        assert_eq!(request.headers.len(), 1);
        assert_eq!(request.headers[0].name, "Accept");
        assert_eq!(request.headers[0].value, "application/dns-message");

        let (_, dns_param) = request
            .url
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
    fn parse_nxdomain() {
        let response = make_dns_message_response(ResponseCode::NXDomain, []);
        let err = parse_response(&response).unwrap_err();
        assert_eq!(err.kind(), ProviderErrorKind::NotFound);
        assert!(err.to_string().contains("Non-Existent Domain"));
    }

    #[test]
    fn resolvers_file_format() {
        let cfg = default_config();
        let content = cfg.format_resolvers_file();
        assert!(content.contains("cloudflare"));
        assert!(content.contains("google"));
        assert!(content.contains(CLOUDFLARE_DOH));
    }
}
