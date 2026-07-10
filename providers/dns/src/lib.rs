#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use std::collections::BTreeMap;
use std::net::IpAddr;

use omnifs_sdk::prelude::*;

mod resolver;

use resolver::{DomainName, ResolverConfig, ResolverName, read_record_bytes, read_reverse_bytes};

use crate::resolver::SupportedRecordType;

#[derive(Clone)]
pub(crate) struct State {
    pub resolvers: ResolverConfig,
}

#[derive(Clone, Debug)]
pub(crate) struct DnsRecord {
    pub rtype: resolver::SupportedRecordType,
    pub value: String,
}

#[omnifs_sdk::config]
struct Config {
    #[omnifs(default = "cloudflare")]
    #[serde(default = "default_resolver_name")]
    default_resolver: String,
    #[serde(default)]
    resolvers: BTreeMap<String, ConfigResolver>,
}

fn default_resolver_name() -> String {
    String::from("cloudflare")
}

#[omnifs_sdk::config]
pub(crate) struct ConfigResolver {
    url: String,
    #[serde(default)]
    aliases: Vec<String>,
}

// ===========================================================================
// Path captures (§8). Each multi-segment route binds a `#[path_captures]` key;
// the prefix capture `@{resolver}` strips the `@` before parsing.
// ===========================================================================

// Dir-handler keys carry their captures for parse-time validation only (the
// handler lists static record types and does not read the fields).
#[omnifs_sdk::path_captures]
struct DomainKey {
    domain: DomainName,
}

#[omnifs_sdk::path_captures]
struct DomainRecordKey {
    domain: DomainName,
    record: String,
}

#[omnifs_sdk::path_captures]
struct ReverseKey {
    ip: IpAddr,
}

#[omnifs_sdk::path_captures]
struct ResolverKey {
    resolver: ResolverName,
}

#[omnifs_sdk::path_captures]
struct ResolverDomainKey {
    resolver: ResolverName,
    domain: DomainName,
}

#[omnifs_sdk::path_captures]
struct ResolverDomainRecordKey {
    resolver: ResolverName,
    domain: DomainName,
    record: String,
}

#[omnifs_sdk::path_captures]
struct ResolverReverseKey {
    resolver: ResolverName,
    ip: IpAddr,
}

// ===========================================================================
// Provider. Capabilities and limits are declared in `#[omnifs_sdk::provider]`;
// no `events(..)` needed.
// ===========================================================================

#[omnifs_sdk::provider(
    id = "dns",
    display_name = "DNS",
    description = "DNS records for any domain",
    mount = "dns",
    capabilities(
        domain(
            "cloudflare-dns.com",
            "Send DNS-over-HTTPS queries to the bundled public resolver endpoints."
        ),
        domain(
            "dns.google",
            "Send DNS-over-HTTPS queries to the bundled public resolver endpoints."
        ),
    ),
    limits(memory_mb(
        32,
        "Keep resolver execution small; DNS responses are compact and do not need a large heap."
    ),)
)]
impl DnsProvider {
    fn start(config: Config, r: &mut Router<State>) -> Result<State> {
        let resolvers = ResolverConfig::from_config(config.default_resolver, config.resolvers)?;

        // Default resolver paths.
        r.dir("/").handler(root_list)?;
        r.file("/resolvers").handler(|cx: Cx<State>| async move {
            let body = cx.state(|state| {
                state
                    .resolvers
                    .entries()
                    .iter()
                    .map(|entry| {
                        format!("{}\t{}\t{}", entry.name, entry.aliases.join(","), entry.url)
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
                    + "\n"
            });
            Ok(FileProjection::body(body.into_bytes()).build())
        })?;
        r.dir("/{domain}")
            .handler(|_cx: DirCx<State>, _key: DomainKey| async move { Ok(record_projection()) })?;
        r.file("/{domain}/{record}")
            .handler(|cx: Cx<State>, key: DomainRecordKey| async move {
                let bytes = read_record_bytes(&cx, None, &key.domain, &key.record).await?;
                Ok(FileProjection::body(bytes)
                    .dynamic()
                    .content_type(ContentType::Text)
                    .build())
            })?;
        r.dir("/reverse")
            .handler(|_cx: DirCx<State>| async move { Ok(open_dir()) })?;
        r.file("/reverse/{ip}")
            .handler(|cx: Cx<State>, key: ReverseKey| async move {
                let ip = key.ip.to_string();
                let bytes = read_reverse_bytes(&cx, None, &ip).await?;
                Ok(FileProjection::body(bytes)
                    .dynamic()
                    .content_type(ContentType::Text)
                    .build())
            })?;

        // Per-resolver paths under `/@{resolver}`.
        r.dir("/@{resolver}")
            .handler(|_cx: DirCx<State>, _key: ResolverKey| async move {
                // Domains are typed, not listed; only the literal `reverse` sibling appears.
                Ok(open_dir())
            })?;
        r.dir("/@{resolver}/reverse")
            .handler(|_cx: DirCx<State>, _key: ResolverKey| async move { Ok(open_dir()) })?;
        r.file("/@{resolver}/reverse/{ip}").handler(
            |cx: Cx<State>, key: ResolverReverseKey| async move {
                let ip = key.ip.to_string();
                let bytes = read_reverse_bytes(&cx, Some(&key.resolver), &ip).await?;
                Ok(FileProjection::body(bytes)
                    .dynamic()
                    .content_type(ContentType::Text)
                    .build())
            },
        )?;
        r.dir("/@{resolver}/{domain}").handler(
            |_cx: DirCx<State>, _key: ResolverDomainKey| async move { Ok(record_projection()) },
        )?;
        r.file("/@{resolver}/{domain}/{record}").handler(
            |cx: Cx<State>, key: ResolverDomainRecordKey| async move {
                let bytes =
                    read_record_bytes(&cx, Some(&key.resolver), &key.domain, &key.record).await?;
                Ok(FileProjection::body(bytes)
                    .dynamic()
                    .content_type(ContentType::Text)
                    .build())
            },
        )?;

        Ok(State { resolvers })
    }
}

async fn root_list(cx: DirCx<State>) -> Result<DirListing> {
    let resolvers = cx.state(|state| {
        state
            .resolvers
            .entries()
            .iter()
            .map(|entry| {
                entry
                    .name
                    .parse::<ResolverName>()
                    .map(|resolver| format!("@{resolver}"))
                    .map_err(|()| {
                        ProviderError::internal(format!(
                            "configured resolver name is invalid: {}",
                            entry.name
                        ))
                    })
            })
            .collect::<Result<Vec<_>>>()
    })?;
    // Open: the literal `resolvers`/`reverse` siblings are merged by the router.
    Ok(DirListing::open(resolvers.into_iter().map(Entry::dir)))
}

/// The exhaustive record-type listing for a domain directory.
fn record_projection() -> DirListing {
    DirListing::exhaustive(
        SupportedRecordType::all()
            .iter()
            .map(|record_type| Entry::file(record_type.as_str()))
            .chain(["all", "raw"].into_iter().map(Entry::file)),
    )
}

/// An open (dynamic, non-exhaustive) directory with no statically-listed
/// children; the router merges any literal siblings and resolves captures
/// (an IP or domain) on demand.
fn open_dir() -> DirListing {
    DirListing::open(core::iter::empty::<Entry>())
}
