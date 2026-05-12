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
    Ok(format_record_lines(&records))
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
            Ok(format_record_lines(&records))
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

    let all_records = collect_query_all_results(responses.into_iter().map(|response| {
        response
            .and_then(ResponseExt::error_for_status)
            .and_then(|resp| doh::parse_response(resp.body()))
    }))?;

    Ok(format_record_lines(&all_records))
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

fn collect_query_all_results(
    results: impl IntoIterator<Item = Result<(Vec<DnsRecord>, u64)>>,
) -> Result<Vec<DnsRecord>> {
    let mut all_records = Vec::new();
    let mut first_error = None;
    let mut rate_limited_error = None;
    let mut had_success = false;

    for result in results {
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

    if had_success {
        return Ok(all_records);
    }

    Err(rate_limited_error
        .or(first_error)
        .unwrap_or_else(|| ProviderError::internal("no DNS record types configured")))
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
    use crate::types::RecordType;

    fn a_record(value: &str) -> DnsRecord {
        DnsRecord {
            rtype: RecordType::A,
            value: value.to_string(),
        }
    }

    #[test]
    fn query_all_keeps_partial_success_when_a_record_type_is_rate_limited() {
        let records = collect_query_all_results(vec![
            Ok((vec![a_record("93.184.216.34")], 300)),
            Err(ProviderError::rate_limited("HTTP 429")),
        ])
        .unwrap();

        assert_eq!(
            String::from_utf8(format_record_lines(&records)).unwrap(),
            "A\t93.184.216.34\n"
        );
    }

    #[test]
    fn query_all_prefers_rate_limited_when_every_record_type_fails() {
        let error = collect_query_all_results(vec![
            Err(ProviderError::internal("resolver failed")),
            Err(ProviderError::rate_limited("HTTP 429")),
        ])
        .unwrap_err();

        assert_eq!(error.kind(), ProviderErrorKind::RateLimited);
    }
}
