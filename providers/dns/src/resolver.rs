//! DNS resolution: validators, `DoH` fetches, and record queries.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::net::IpAddr;
use std::str::FromStr;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hickory_proto::op::{Message, MessageType, OpCode, Query as DnsQuery, ResponseCode};
use hickory_proto::rr::Name;
use hickory_proto::rr::RecordType as HickoryRecordType;

use omnifs_sdk::Cx;
use omnifs_sdk::prelude::*;

use crate::{DnsRecord, State};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct SupportedRecordType(HickoryRecordType);

impl SupportedRecordType {
    const SUPPORTED: &'static [Self] = &[
        Self::A,
        Self::AAAA,
        Self::CNAME,
        Self::MX,
        Self::NS,
        Self::TXT,
        Self::SOA,
        Self::SRV,
        Self::CAA,
        Self::PTR,
    ];

    pub const A: Self = Self(HickoryRecordType::A);
    pub const AAAA: Self = Self(HickoryRecordType::AAAA);
    pub const CNAME: Self = Self(HickoryRecordType::CNAME);
    pub const MX: Self = Self(HickoryRecordType::MX);
    pub const NS: Self = Self(HickoryRecordType::NS);
    pub const TXT: Self = Self(HickoryRecordType::TXT);
    pub const SOA: Self = Self(HickoryRecordType::SOA);
    pub const SRV: Self = Self(HickoryRecordType::SRV);
    pub const CAA: Self = Self(HickoryRecordType::CAA);
    pub const PTR: Self = Self(HickoryRecordType::PTR);

    /// PTR excluded: it is only used internally for `reverse/<ip>`.
    pub fn all() -> &'static [Self] {
        &[
            Self::A,
            Self::AAAA,
            Self::CNAME,
            Self::MX,
            Self::NS,
            Self::TXT,
            Self::SOA,
            Self::SRV,
            Self::CAA,
        ]
    }

    /// Subset queried in parallel for `all` (skip SRV/CAA to reduce noise).
    pub fn common() -> &'static [Self] {
        &[
            Self::A,
            Self::AAAA,
            Self::CNAME,
            Self::MX,
            Self::NS,
            Self::TXT,
            Self::SOA,
        ]
    }

    pub fn from_hickory(rtype: HickoryRecordType) -> Option<Self> {
        Self::SUPPORTED
            .iter()
            .copied()
            .find(|supported| supported.0 == rtype)
    }

    pub fn as_hickory(self) -> HickoryRecordType {
        self.0
    }

    pub fn as_str(self) -> &'static str {
        self.0.into()
    }
}

impl FromStr for SupportedRecordType {
    type Err = ();

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        value
            .parse::<HickoryRecordType>()
            .ok()
            .and_then(Self::from_hickory)
            .ok_or(())
    }
}

impl std::fmt::Display for SupportedRecordType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl AsRef<str> for SupportedRecordType {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

fn is_valid_domain_name(s: &str) -> bool {
    s.parse::<IpAddr>().is_err()
        && s.contains('.')
        && !s.contains(char::is_whitespace)
        && s.len() <= 253
}

fn is_valid_resolver_name(s: &str) -> bool {
    !s.is_empty() && !s.contains('/') && !s.contains(char::is_whitespace)
}

#[omnifs_sdk::path_segment(validate = is_valid_domain_name)]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct DomainName(String);

#[omnifs_sdk::path_segment(validate = is_valid_resolver_name)]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ResolverName(String);

const BUILTIN_RESOLVERS: &[(&str, &str, &[&str])] = &[
    (
        "cloudflare",
        "https://cloudflare-dns.com/dns-query",
        &["1.1.1.1", "1.0.0.1"],
    ),
    (
        "google",
        "https://dns.google/dns-query",
        &["8.8.8.8", "8.8.4.4", "dns.google"],
    ),
];

struct RawResolver {
    url: String,
    aliases: Vec<String>,
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

fn builtin_resolver_entries() -> Result<Vec<ResolverEntry>> {
    BUILTIN_RESOLVERS
        .iter()
        .map(|(name, url, aliases)| {
            Ok(ResolverEntry {
                name: (*name).to_string(),
                url: Endpoint::new(*url).map_err(ProviderError::invalid_input)?,
                aliases: aliases.iter().map(|alias| (*alias).to_string()).collect(),
            })
        })
        .collect()
}

/// The `DoH` resolver endpoint. The base is the fully-formed query URL
/// (resolver URL plus the `dns=` parameter, built by [`query_url`]), so the
/// request path is empty and the endpoint URL builder uses the base verbatim.
/// Routing through the endpoint gives every resolver the rate-limit breaker.
struct DohEndpoint {
    base: String,
}

impl omnifs_sdk::endpoint::Endpoint for DohEndpoint {
    fn base(&self) -> &str {
        &self.base
    }
}
impl omnifs_sdk::endpoint::EndpointHooks for DohEndpoint {}

async fn fetch_dns_message(cx: &Cx<State>, url: String) -> Result<Vec<u8>> {
    let response = cx
        .endpoint(DohEndpoint { base: url })
        .get("")
        .header("Accept", "application/dns-message")
        .send_checked()
        .await?;
    Ok(response.body().to_vec())
}

/// Validated `DoH` endpoint URL (always HTTPS).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Endpoint(String);

impl Endpoint {
    fn new(url: impl Into<String>) -> std::result::Result<Self, String> {
        let url = url.into();
        if !url.starts_with("https://") {
            return Err(format!("DoH endpoint must use HTTPS: {url}"));
        }
        Ok(Self(url))
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
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
pub(super) struct ResolverEntry {
    pub(super) name: String,
    pub(super) url: Endpoint,
    pub(super) aliases: Vec<String>,
}

impl ResolverConfig {
    pub(super) fn entries(&self) -> &[ResolverEntry] {
        &self.resolvers
    }

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
            builtin_resolver_entries()?
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
}

/// Build a `DoH` query URL for the new async SDK (returns URL string).
pub(super) fn query_url(
    config: &ResolverConfig,
    resolver: Option<&str>,
    domain: &str,
    rtype: SupportedRecordType,
) -> Result<String> {
    let fqdn = if domain.ends_with('.') {
        domain.to_string()
    } else {
        format!("{domain}.")
    };
    let name = Name::from_ascii(&fqdn).map_err(|error| {
        ProviderError::invalid_input(format!("invalid domain name {domain:?}: {error}"))
    })?;
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

fn query_with_name(
    config: &ResolverConfig,
    resolver: Option<&str>,
    name: Name,
    rtype: SupportedRecordType,
) -> Result<String> {
    let endpoint = match resolver {
        None => config.default_endpoint()?,
        Some(spec) if spec.contains("://") => {
            Endpoint::new(spec).map_err(ProviderError::invalid_input)?
        },
        Some(spec) => config.lookup(spec).ok_or_else(|| {
            ProviderError::invalid_input(format!("unknown resolver specifier: {spec}"))
        })?,
    };
    let ep = &endpoint.0;
    let sep = if ep.contains('?') { '&' } else { '?' };

    let mut message = Message::new(0, MessageType::Query, OpCode::Query);
    message.add_query(DnsQuery::query(name.clone(), rtype.as_hickory()));
    message.metadata.recursion_desired = true;
    let wire = message
        .to_vec()
        .map_err(|error| ProviderError::internal(format!("failed to encode DNS query: {error}")))?;
    let dns_query = URL_SAFE_NO_PAD.encode(wire);

    Ok(format!("{ep}{sep}dns={dns_query}"))
}

pub(crate) async fn read_reverse_bytes(
    cx: &Cx<State>,
    resolver: Option<&ResolverName>,
    ip: &str,
) -> Result<Vec<u8>> {
    let config = cx.state(|s| s.resolvers.clone());
    let resolver_name = resolver.map(ResolverName::as_ref);
    let addr = ip
        .parse::<IpAddr>()
        .map_err(|_| ProviderError::invalid_input(format!("invalid IP address: {ip}")))?;
    let name = Name::from(addr);
    let url = query_with_name(&config, resolver_name, name, SupportedRecordType::PTR)?;
    let body = fetch_dns_message(cx, url).await?;
    let (records, _) = parse_response(&body)?;
    Ok(format_record_lines(&records))
}

pub(crate) async fn read_record_bytes(
    cx: &Cx<State>,
    resolver: Option<&ResolverName>,
    domain: &DomainName,
    record: &str,
) -> Result<Vec<u8>> {
    let config = cx.state(|s| s.resolvers.clone());
    match record {
        "all" => {
            let domain_str = domain.to_string();
            let resolver_ref = resolver.map(ResolverName::as_ref);

            let mut requests = Vec::with_capacity(SupportedRecordType::common().len());
            for record_type in SupportedRecordType::common() {
                let url = query_url(&config, resolver_ref, &domain_str, *record_type)?;
                requests.push(fetch_dns_message(cx, url));
            }

            let responses = join_all(requests).await;

            let mut all_records = Vec::new();
            let mut first_error = None;
            let mut rate_limited_error = None;
            let mut had_success = false;

            for response in responses {
                let result = response.and_then(|body| parse_response(&body));
                match result {
                    Ok((records, _)) => {
                        had_success = true;
                        all_records.extend(records);
                    },
                    Err(error) => {
                        if error.kind() == ProviderErrorKind::RateLimited {
                            rate_limited_error.get_or_insert(error);
                            continue;
                        }
                        first_error.get_or_insert(error);
                    },
                }
            }

            if !had_success {
                return Err(rate_limited_error
                    .or(first_error)
                    .unwrap_or_else(|| ProviderError::internal("no DNS record types configured")));
            }

            Ok(format_record_lines(&all_records))
        },
        "raw" => {
            let domain_str = domain.to_string();
            let resolver_ref = resolver.map(ResolverName::as_ref);
            let url = query_url(&config, resolver_ref, &domain_str, SupportedRecordType::A)?;
            let body = fetch_dns_message(cx, url).await?;
            let (records, _) = parse_response(&body)?;

            let mut out = String::new();
            let _ = writeln!(out, ";; QUESTION SECTION:");
            let _ = writeln!(out, ";{domain_str}.\t\tIN\tA");
            let _ = writeln!(out);
            let _ = writeln!(out, ";; ANSWER SECTION:");
            for r in &records {
                let _ = writeln!(out, "{domain_str}.\t\tIN\t{}\t{}", r.rtype, r.value);
            }
            let _ = writeln!(out);
            let _ = writeln!(out, ";; RECORDS: {}", records.len());
            Ok(out.into_bytes())
        },
        other => {
            let record_type = other
                .parse::<SupportedRecordType>()
                .map_err(|()| ProviderError::not_found("record not found"))?;
            let domain_str = domain.to_string();
            let resolver_name = resolver.map(ResolverName::as_ref);
            let url = query_url(&config, resolver_name, &domain_str, record_type)?;
            let body = fetch_dns_message(cx, url).await?;
            let (records, _) = parse_response(&body)?;
            Ok(format_record_lines(&records))
        },
    }
}

fn format_record_lines(records: &[DnsRecord]) -> Vec<u8> {
    let mut output = String::new();
    for r in records {
        let _ = writeln!(output, "{}\t{}", r.rtype, r.value);
    }
    output.into_bytes()
}
