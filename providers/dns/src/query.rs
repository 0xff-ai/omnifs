use omnifs_sdk::Cx;
use omnifs_sdk::http::ResponseExt;
use omnifs_sdk::prelude::*;
use std::fmt::Write;

use crate::doh;
use crate::http_ext::DnsHttpExt;
use crate::types::{DomainName, RecordType, ResolverName};
use crate::{DnsRecord, State};

pub(crate) fn record_names() -> Vec<String> {
    let mut names: Vec<String> = RecordType::all()
        .iter()
        .map(|rt| rt.as_ref().to_string())
        .collect();
    names.push("_all".to_string());
    names.push("_raw".to_string());
    names
}

pub(crate) async fn read_reverse_bytes(
    cx: &Cx<State>,
    resolver: Option<&ResolverName>,
    ip: &str,
) -> Result<Vec<u8>> {
    let resolver_name = resolver.map(ResolverName::as_ref);
    let url = cx.state(|s| doh::reverse_query_url(&s.resolvers, resolver_name, ip))?;
    let resp = cx.dns_message_get(url).send().await?.error_for_status()?;
    let (records, _) = doh::parse_response(resp.body())?;
    Ok(format_records(&records).into_bytes())
}

pub(crate) async fn read_record_bytes(
    cx: &Cx<State>,
    resolver: Option<&ResolverName>,
    domain: &DomainName,
    record: &str,
) -> Result<Vec<u8>> {
    match record {
        "_all" => query_all(cx, resolver, domain).await,
        "_raw" => query_raw(cx, resolver, domain).await,
        other => {
            let record_type = other
                .parse::<RecordType>()
                .map_err(|_| ProviderError::not_found("record not found"))?;
            let domain_str = domain.to_string();
            let resolver_name = resolver.map(ResolverName::as_ref);
            let url = cx
                .state(|s| doh::query_url(&s.resolvers, resolver_name, &domain_str, record_type))?;
            let resp = cx.dns_message_get(url).send().await?.error_for_status()?;
            let (records, _) = doh::parse_response(resp.body())?;
            Ok(format_records(&records).into_bytes())
        },
    }
}

/// Query all common record types. The per-type `DoH` requests are
/// independent, so they are batched into a single host round trip via
/// `join_all` and the host runs them in parallel.
pub(crate) async fn query_all(
    cx: &Cx<State>,
    resolver: Option<&ResolverName>,
    domain: &DomainName,
) -> Result<Vec<u8>> {
    let domain_str = domain.to_string();
    let resolver_ref = resolver.map(ResolverName::as_ref);

    let mut requests = Vec::with_capacity(RecordType::common().len());
    for record_type in RecordType::common() {
        let url =
            cx.state(|s| doh::query_url(&s.resolvers, resolver_ref, &domain_str, *record_type))?;
        requests.push(cx.dns_message_get(url).send());
    }

    let responses = join_all(requests).await;

    let mut all_records = Vec::new();
    let mut first_error = None;
    let mut had_success = false;
    for response in responses {
        let result = response
            .and_then(ResponseExt::error_for_status)
            .and_then(|resp| doh::parse_response(resp.body()));
        match result {
            Ok((records, _)) => {
                had_success = true;
                all_records.extend(records);
            },
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            },
        }
    }

    if !had_success {
        return Err(match first_error {
            Some(error) => error,
            None => ProviderError::internal("no DNS record types configured"),
        });
    }

    let mut output = String::new();
    for r in &all_records {
        let _ = writeln!(output, "{}\t{}", r.rtype, r.value);
    }
    Ok(output.into_bytes())
}

/// Query the A record for `domain` and render the response in
/// `dig(1)`-style sections. The hex dump it used to emit was opaque;
/// a formatted ANSWER section is the shape users inspecting `_raw`
/// actually want.
pub(crate) async fn query_raw(
    cx: &Cx<State>,
    resolver: Option<&ResolverName>,
    domain: &DomainName,
) -> Result<Vec<u8>> {
    let domain_str = domain.to_string();
    let resolver_ref = resolver.map(ResolverName::as_ref);
    let url =
        cx.state(|s| doh::query_url(&s.resolvers, resolver_ref, &domain_str, RecordType::A))?;
    let resp = cx.dns_message_get(url).send().await?.error_for_status()?;
    let (records, _) = doh::parse_response(resp.body())?;

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
}

fn format_records(records: &[DnsRecord]) -> String {
    records
        .iter()
        .map(|r| format!("{}\t{}", r.rtype, r.value))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}
