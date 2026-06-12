//! DNS resolution: validators, `DoH` transport, and record queries.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::net::IpAddr;
use std::str::FromStr;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hickory_proto::op::{Message, MessageType, OpCode, Query as DnsQuery, ResponseCode};
use hickory_proto::rr::Name;
use hickory_proto::rr::RecordType as HickoryRecordType;

use omnifs_sdk::Cx;
use omnifs_sdk::http::ResponseExt;
use omnifs_sdk::prelude::*;
use omnifs_sdk::serde::Deserialize;

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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct DomainName(String);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ResolverName(String);

impl FromStr for DomainName {
    type Err = ();

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        (s.parse::<IpAddr>().is_err()
            && s.contains('.')
            && !s.contains(char::is_whitespace)
            && s.len() <= 253)
            .then_some(Self(s.to_string()))
            .ok_or(())
    }
}

impl std::fmt::Display for DomainName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl AsRef<str> for DomainName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl FromStr for ResolverName {
    type Err = ();

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        (!s.is_empty() && !s.contains('/') && !s.contains(char::is_whitespace))
            .then_some(Self(s.to_string()))
            .ok_or(())
    }
}

impl std::fmt::Display for ResolverName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl AsRef<str> for ResolverName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Provider-local `DoH` transport boundary: fetch a prepared `DoH` URL and
/// return the raw DNS wire-format response body.
///
/// Production goes through the SDK's `Callout::Fetch` via the [`Cx<State>`]
/// impl below; tests inject canned responses without touching the WIT
/// contract.
pub(crate) trait DohTransport {
    async fn fetch_dns_message(&self, url: String) -> Result<Vec<u8>>;
}

impl DohTransport for Cx<State> {
    async fn fetch_dns_message(&self, url: String) -> Result<Vec<u8>> {
        let response = self
            .http()
            .get(url)
            .header("Accept", "application/dns-message")
            .send()
            .await?
            .error_for_status()?;
        Ok(response.into_body())
    }
}

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
            let raw: RawConfig = omnifs_sdk::serde_json::from_slice(
                BUILTIN_DEFAULTS_JSON.as_bytes(),
            )
            .map_err(|error| {
                ProviderError::invalid_input(format!("invalid resolver config: {error}"))
            })?;
            build_resolver_entries(raw.resolvers)?
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
    read_reverse_bytes_with(cx, &config, resolver, ip).await
}

async fn read_reverse_bytes_with<T: DohTransport>(
    transport: &T,
    config: &ResolverConfig,
    resolver: Option<&ResolverName>,
    ip: &str,
) -> Result<Vec<u8>> {
    let resolver_name = resolver.map(ResolverName::as_ref);
    let addr = ip
        .parse::<IpAddr>()
        .map_err(|_| ProviderError::invalid_input(format!("invalid IP address: {ip}")))?;
    let name = Name::from(addr);
    let url = query_with_name(config, resolver_name, name, SupportedRecordType::PTR)?;
    let body = transport.fetch_dns_message(url).await?;
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
    read_record_bytes_with(cx, &config, resolver, domain, record).await
}

async fn read_record_bytes_with<T: DohTransport>(
    transport: &T,
    config: &ResolverConfig,
    resolver: Option<&ResolverName>,
    domain: &DomainName,
    record: &str,
) -> Result<Vec<u8>> {
    match record {
        "all" => {
            let domain_str = domain.to_string();
            let resolver_ref = resolver.map(ResolverName::as_ref);

            let mut requests = Vec::with_capacity(SupportedRecordType::common().len());
            for record_type in SupportedRecordType::common() {
                let url = query_url(config, resolver_ref, &domain_str, *record_type)?;
                requests.push(transport.fetch_dns_message(url));
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
            let url = query_url(config, resolver_ref, &domain_str, SupportedRecordType::A)?;
            let body = transport.fetch_dns_message(url).await?;
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
            let url = query_url(config, resolver_name, &domain_str, record_type)?;
            let body = transport.fetch_dns_message(url).await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use hickory_proto::rr::rdata::{A, PTR, TXT};
    use hickory_proto::rr::{RData, Record};
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::net::Ipv4Addr;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    /// Drive a future whose only awaits are mock transport calls. Mock
    /// futures are ready on first poll, so `Pending` means the future
    /// reached a real callout and would stall forever in a test.
    fn block_on<F: Future>(fut: F) -> F::Output {
        let mut fut = pin!(fut);
        let mut ctx = Context::from_waker(Waker::noop());
        match fut.as_mut().poll(&mut ctx) {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("test future stalled: mock transport futures must be ready"),
        }
    }

    fn config() -> ResolverConfig {
        ResolverConfig::from_config(
            crate::default_resolver_name(),
            std::iter::empty::<(String, crate::ConfigResolver)>(),
        )
        .expect("builtin defaults parse")
    }

    fn domain(s: &str) -> DomainName {
        s.parse().expect("valid test domain")
    }

    fn resolver(s: &str) -> ResolverName {
        s.parse().expect("valid test resolver name")
    }

    /// Read `/example.com/{record}` through the mock, optionally via a named
    /// resolver. Wraps the `block_on`/`config` plumbing shared by every test.
    fn read(mock: &MockDoh, resolver_spec: Option<&str>, record: &str) -> Result<Vec<u8>> {
        let resolver_name = resolver_spec.map(resolver);
        block_on(read_record_bytes_with(
            mock,
            &config(),
            resolver_name.as_ref(),
            &domain("example.com"),
            record,
        ))
    }

    fn a_record(name: &Name, ip: [u8; 4]) -> Record {
        Record::from_rdata(
            name.clone(),
            300,
            RData::A(A(Ipv4Addr::new(ip[0], ip[1], ip[2], ip[3]))),
        )
    }

    /// Encode a DNS wire-format response the way a DoH resolver would.
    fn doh_response(code: ResponseCode, answers: Vec<Record>) -> Vec<u8> {
        let mut message = Message::new(0, MessageType::Response, OpCode::Query);
        message.metadata.response_code = code;
        message.answers = answers;
        message.to_vec().expect("encode mock DNS response")
    }

    enum Reply {
        Answers(Vec<Record>),
        Rcode(ResponseCode),
        Error(ProviderError),
    }

    /// One observed DoH request, decoded from the `dns=` query parameter.
    struct SeenQuery {
        endpoint: String,
        qname: Name,
        qtype: HickoryRecordType,
    }

    /// Mock transport keyed by query record type. Unmatched types resolve
    /// to an empty `NoError` answer, mirroring a domain with no records.
    struct MockDoh {
        replies: BTreeMap<HickoryRecordType, Reply>,
        seen: RefCell<Vec<SeenQuery>>,
    }

    impl MockDoh {
        fn new(replies: impl IntoIterator<Item = (SupportedRecordType, Reply)>) -> Self {
            Self {
                replies: replies
                    .into_iter()
                    .map(|(rtype, reply)| (rtype.as_hickory(), reply))
                    .collect(),
                seen: RefCell::new(Vec::new()),
            }
        }

        fn seen(&self) -> std::cell::Ref<'_, Vec<SeenQuery>> {
            self.seen.borrow()
        }
    }

    impl DohTransport for MockDoh {
        async fn fetch_dns_message(&self, url: String) -> Result<Vec<u8>> {
            let (endpoint, dns_param) = url
                .split_once("?dns=")
                .expect("DoH URL must carry a dns query parameter");
            let wire = URL_SAFE_NO_PAD
                .decode(dns_param)
                .expect("dns parameter is URL-safe base64");
            let query_message =
                Message::from_vec(&wire).expect("dns parameter decodes to a DNS message");
            let query = query_message
                .queries
                .first()
                .expect("DoH query carries one question");
            let qtype = query.query_type();
            self.seen.borrow_mut().push(SeenQuery {
                endpoint: endpoint.to_string(),
                qname: query.name().clone(),
                qtype,
            });
            match self.replies.get(&qtype) {
                Some(Reply::Answers(records)) => {
                    Ok(doh_response(ResponseCode::NoError, records.clone()))
                },
                Some(Reply::Rcode(code)) => Ok(doh_response(*code, Vec::new())),
                Some(Reply::Error(error)) => Err(error.clone()),
                None => Ok(doh_response(ResponseCode::NoError, Vec::new())),
            }
        }
    }

    fn example_fqdn() -> Name {
        Name::from_ascii("example.com.").expect("valid name")
    }

    #[test]
    fn single_record_read_queries_default_resolver() {
        let mock = MockDoh::new([(
            SupportedRecordType::A,
            Reply::Answers(vec![a_record(&example_fqdn(), [93, 184, 216, 34])]),
        )]);
        let output = read(&mock, None, "A").expect("read succeeds");
        assert_eq!(output, b"A\t93.184.216.34\n");

        let seen = mock.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].endpoint, "https://cloudflare-dns.com/dns-query");
        assert_eq!(seen[0].qname, example_fqdn());
        assert_eq!(seen[0].qtype, HickoryRecordType::A);
    }

    #[test]
    fn named_resolver_and_alias_select_endpoint() {
        for spec in ["google", "8.8.8.8"] {
            let mock = MockDoh::new([]);
            read(&mock, Some(spec), "A").expect("read succeeds");
            assert_eq!(mock.seen()[0].endpoint, "https://dns.google/dns-query");
        }
    }

    #[test]
    fn url_spec_resolver_is_used_verbatim() {
        let url = query_url(
            &config(),
            Some("https://doh.example/dns-query"),
            "example.com",
            SupportedRecordType::A,
        )
        .expect("url builds");
        assert!(url.starts_with("https://doh.example/dns-query?dns="));
    }

    #[test]
    fn unknown_resolver_spec_is_invalid_input() {
        let mock = MockDoh::new([]);
        let error = read(&mock, Some("quad9"), "A").expect_err("unknown resolver rejected");
        assert_eq!(error.kind(), ProviderErrorKind::InvalidInput);
        assert!(mock.seen().is_empty());
    }

    #[test]
    fn unknown_record_type_is_not_found_without_query() {
        let mock = MockDoh::new([]);
        let error = read(&mock, None, "BOGUS").expect_err("unsupported record type rejected");
        assert_eq!(error.kind(), ProviderErrorKind::NotFound);
        assert!(mock.seen().is_empty());
    }

    #[test]
    fn response_codes_map_to_error_kinds() {
        for (rcode, kind) in [
            (ResponseCode::NXDomain, ProviderErrorKind::NotFound),
            (ResponseCode::ServFail, ProviderErrorKind::Network),
        ] {
            let mock = MockDoh::new([(SupportedRecordType::A, Reply::Rcode(rcode))]);
            let error = read(&mock, None, "A").expect_err("rcode surfaces as an error");
            assert_eq!(error.kind(), kind, "{rcode}");
        }
    }

    #[test]
    fn all_merges_results_and_tolerates_partial_failures() {
        let mock = MockDoh::new([
            (
                SupportedRecordType::A,
                Reply::Answers(vec![a_record(&example_fqdn(), [93, 184, 216, 34])]),
            ),
            (
                SupportedRecordType::TXT,
                Reply::Error(ProviderError::network("mock network failure")),
            ),
        ]);
        let output = read(&mock, None, "all").expect("partial success still succeeds");
        let text = String::from_utf8(output).expect("utf8 output");
        assert!(
            text.contains("A\t93.184.216.34"),
            "missing A record: {text}"
        );
        assert_eq!(mock.seen().len(), SupportedRecordType::common().len());
    }

    #[test]
    fn all_prefers_rate_limited_error_when_nothing_succeeds() {
        let mock = MockDoh::new(SupportedRecordType::common().iter().map(|rtype| {
            let error = if *rtype == SupportedRecordType::TXT {
                ProviderError::rate_limited("mock 429")
            } else {
                ProviderError::network("mock network failure")
            };
            (*rtype, Reply::Error(error))
        }));
        let error = read(&mock, None, "all").expect_err("total failure surfaces an error");
        assert_eq!(error.kind(), ProviderErrorKind::RateLimited);
    }

    #[test]
    fn all_surfaces_first_error_when_no_rate_limit() {
        let mock = MockDoh::new(SupportedRecordType::common().iter().map(|rtype| {
            (
                *rtype,
                Reply::Error(ProviderError::network("mock network failure")),
            )
        }));
        let error = read(&mock, None, "all").expect_err("total failure surfaces an error");
        assert_eq!(error.kind(), ProviderErrorKind::Network);
    }

    #[test]
    fn raw_renders_question_and_answer_sections() {
        let mock = MockDoh::new([(
            SupportedRecordType::A,
            Reply::Answers(vec![a_record(&example_fqdn(), [93, 184, 216, 34])]),
        )]);
        let output = read(&mock, None, "raw").expect("raw read succeeds");
        let text = String::from_utf8(output).expect("utf8 output");
        assert!(text.contains(";; QUESTION SECTION:"), "{text}");
        assert!(
            text.contains("example.com.\t\tIN\tA\t93.184.216.34"),
            "{text}"
        );
        assert!(text.contains(";; RECORDS: 1"), "{text}");
    }

    #[test]
    fn reverse_lookup_queries_ptr_for_in_addr_arpa() {
        let ptr_name = Name::from_ascii("one.one.one.one.").expect("valid name");
        let arpa_name = Name::from_ascii("1.1.1.1.in-addr.arpa.").expect("valid name");
        let mock = MockDoh::new([(
            SupportedRecordType::PTR,
            Reply::Answers(vec![Record::from_rdata(
                arpa_name.clone(),
                300,
                RData::PTR(PTR(ptr_name)),
            )]),
        )]);
        let output = block_on(read_reverse_bytes_with(&mock, &config(), None, "1.1.1.1"))
            .expect("reverse read succeeds");
        let text = String::from_utf8(output).expect("utf8 output");
        assert!(text.starts_with("PTR\tone.one.one.one"), "{text}");

        let seen = mock.seen();
        assert_eq!(seen[0].qname, arpa_name);
        assert_eq!(seen[0].qtype, HickoryRecordType::PTR);
    }

    #[test]
    fn invalid_ip_is_rejected_without_query() {
        let mock = MockDoh::new([]);
        let error = block_on(read_reverse_bytes_with(&mock, &config(), None, "not-an-ip"))
            .expect_err("invalid IP rejected");
        assert_eq!(error.kind(), ProviderErrorKind::InvalidInput);
        assert!(mock.seen().is_empty());
    }

    #[test]
    fn empty_answer_reads_as_empty_output() {
        // Documents current behavior (see issue #56): an empty answer is
        // indistinguishable from missing records in the projected file.
        let mock = MockDoh::new([]);
        let output = read(&mock, None, "A").expect("empty answer still succeeds");
        assert!(output.is_empty());
    }

    #[test]
    fn parse_response_uses_default_ttl_for_empty_answers() {
        let body = doh_response(ResponseCode::NoError, Vec::new());
        let (records, ttl) = parse_response(&body).expect("parses");
        assert!(records.is_empty());
        assert_eq!(ttl, 300);
    }

    #[test]
    fn txt_records_render_with_record_type_prefix() {
        let mock = MockDoh::new([(
            SupportedRecordType::TXT,
            Reply::Answers(vec![Record::from_rdata(
                example_fqdn(),
                300,
                RData::TXT(TXT::new(vec!["v=spf1 -all".to_string()])),
            )]),
        )]);
        let output = read(&mock, None, "TXT").expect("TXT read succeeds");
        let text = String::from_utf8(output).expect("utf8 output");
        assert!(text.starts_with("TXT\t"), "{text}");
        assert!(text.contains("v=spf1 -all"), "{text}");
    }

    #[test]
    fn query_url_rejects_invalid_domain() {
        let error = query_url(&config(), None, "exa mple.com", SupportedRecordType::A)
            .expect_err("invalid domain rejected");
        assert_eq!(error.kind(), ProviderErrorKind::InvalidInput);
    }
}
