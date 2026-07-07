//! Scrub and rewrite rules applied once, at record time.
//!
//! Every rule here runs before a byte reaches disk, so the tape on disk is
//! already scrubbed (credentials removed), normalized (volatile fields fixed),
//! and, when a provider opts in, sanitized (private values hashed). The same
//! request scrub also runs at replay time so record and replay compare
//! scrubbed-to-scrubbed.

use super::{TapeHeader, sha256_hex};
use omnifs_api::events::{is_sensitive_header, is_sensitive_query_param};
use omnifs_wit::provider::types::Header;
use serde_json::Value;

/// Placeholder written in place of any credential-bearing value.
const SCRUBBED: &str = "<scrubbed>";
/// Placeholder written in place of a normalized (volatile) field value.
const VOLATILE: &str = "<volatile>";

/// Per-provider recording rules. Applied once, at record time; the tape on
/// disk is already scrubbed, normalized, and (if opted in) sanitized.
#[derive(Debug, Clone, Copy, Default)]
pub struct TapeRules {
    /// Response headers dropped from tape entries, case-insensitive, on top of
    /// [`BASE_DROPPED_RESPONSE_HEADERS`]. For per-provider volatile headers.
    pub drop_response_headers: &'static [&'static str],
    /// How response bodies are treated.
    pub body: BodyPolicy,
}

/// How a provider's response bodies are persisted.
#[derive(Debug, Clone, Copy, Default)]
pub enum BodyPolicy {
    /// Bytes stored exactly as received. The default and strongly preferred:
    /// upstream formatting quirks must survive into tests.
    #[default]
    Verbatim,
    /// Bodies parsed as `JSON`, rewritten, and re-serialized with
    /// `serde_json::to_string_pretty`. Deterministic but NOT byte-faithful;
    /// only for providers whose responses contain private data that no test
    /// tenant can avoid (e.g. oura biometrics). Non-`JSON` bodies under this
    /// policy are a record-time error.
    RewrittenJson {
        /// Field names (matched at any nesting depth, exact case) whose values
        /// are replaced with `<redacted:{first 8 hex of sha256 of the original
        /// serialized value}>`. Deterministic per value, distinct,
        /// non-reversible.
        sanitize_fields: &'static [&'static str],
        /// Field names whose values are replaced with the fixed string
        /// `<volatile>`. For timestamps/request ids that churn every re-record
        /// without meaning.
        normalize_fields: &'static [&'static str],
    },
}

/// Always dropped from recorded responses: transport noise that churns every
/// re-record and carries no contract meaning. `etag` is NOT here: it is a
/// version token the engine's revalidation semantics depend on.
pub const BASE_DROPPED_RESPONSE_HEADERS: &[&str] = &[
    "date",
    "x-request-id",
    "x-github-request-id",
    "x-served-by",
    "cf-ray",
    "x-ratelimit-remaining",
    "x-ratelimit-reset",
    "x-ratelimit-used",
    "server-timing",
    "age",
    "via",
    "set-cookie",
];

/// An outbound request reduced to what persists and what matches: scrubbed
/// method/url/headers plus a body digest. Produced identically at record time
/// (before writing) and at replay time (before matching).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrubbedRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<TapeHeader>,
    /// `SHA-256` of the request body, present only when the body is non-empty.
    pub body_sha256: Option<String>,
}

/// A recorded response before scrubbing: raw bytes and headers straight off the
/// callout result, plus the resolved blob bytes for a `FetchBlob`.
#[derive(Debug, Clone)]
pub enum RecordedResponse {
    Http {
        status: u16,
        headers: Vec<TapeHeader>,
        body: Vec<u8>,
    },
    Blob {
        status: u16,
        content_type: Option<String>,
        etag: Option<String>,
        response_headers: Vec<TapeHeader>,
        body: Vec<u8>,
    },
    Error {
        kind: String,
        message: String,
        retryable: bool,
    },
}

/// A response after [`rewrite_response`]: dropped headers removed and the body
/// policy applied. The caller encodes the body bytes into a `TapeBody`.
#[derive(Debug, Clone)]
pub enum RewrittenResponse {
    Http {
        status: u16,
        headers: Vec<TapeHeader>,
        body: Vec<u8>,
    },
    Blob {
        status: u16,
        content_type: Option<String>,
        etag: Option<String>,
        response_headers: Vec<TapeHeader>,
        body: Vec<u8>,
    },
    Error {
        kind: String,
        message: String,
        retryable: bool,
    },
}

/// Failures rewriting a recorded response.
#[derive(Debug, thiserror::Error)]
pub enum ScrubError {
    #[error("response body under RewrittenJson policy is not valid JSON: {source}")]
    NonJsonBody { source: serde_json::Error },
}

/// Scrub an outbound request for persistence and matching: sensitive header
/// values ([`is_sensitive_header`]) and sensitive query param values
/// ([`is_sensitive_query_param`]) replaced with `<scrubbed>`; names, order, and
/// everything else preserved. Used identically at record time (before writing)
/// and at replay time (before matching), so the two sides compare
/// scrubbed-to-scrubbed.
///
/// # Panics
///
/// Panics if `url` does not parse. A provider emitting an unparseable URL is a
/// bug to surface, not something to tape.
#[must_use]
pub fn scrub_request(
    method: &str,
    url: &str,
    headers: &[Header],
    body: Option<&[u8]>,
) -> ScrubbedRequest {
    let scrubbed_headers = headers
        .iter()
        .map(|header| TapeHeader {
            name: header.name.clone(),
            value: if is_sensitive_header(&header.name) {
                SCRUBBED.to_owned()
            } else {
                header.value.clone()
            },
        })
        .collect();
    // An empty body must not discriminate matches; treat it as absent.
    let body_sha256 = body.filter(|bytes| !bytes.is_empty()).map(sha256_hex);
    ScrubbedRequest {
        method: method.to_owned(),
        url: scrub_url(url),
        headers: scrubbed_headers,
        body_sha256,
    }
}

/// Replace sensitive query param values with the literal `<scrubbed>`,
/// preserving param order and the exact encoding of every other part of the
/// URL. The rewrite is surgical on the raw query string rather than a
/// decode/re-encode round-trip, so untouched pairs never churn and the sentinel
/// stays readable (the `url` crate's query serializer would percent-encode it).
fn scrub_url(raw: &str) -> String {
    // Validate up front; a provider emitting an unparseable URL is a hard error.
    url::Url::parse(raw).unwrap_or_else(|err| {
        panic!("tape scrub: provider emitted an unparseable URL {raw:?}: {err}")
    });
    let Some((prefix, after)) = raw.split_once('?') else {
        return raw.to_owned();
    };
    // A fragment is not part of the query; keep it untouched.
    let (query, fragment) = match after.split_once('#') {
        Some((query, fragment)) => (query, Some(fragment)),
        None => (after, None),
    };
    let mut changed = false;
    let rewritten: Vec<String> = query
        .split('&')
        .map(|pair| {
            let key_raw = pair.split_once('=').map_or(pair, |(key, _)| key);
            if is_sensitive_query_param(&form_decode_key(key_raw)) {
                changed = true;
                format!("{key_raw}={SCRUBBED}")
            } else {
                pair.to_owned()
            }
        })
        .collect();
    if !changed {
        return raw.to_owned();
    }
    let mut out = format!("{prefix}?{}", rewritten.join("&"));
    if let Some(fragment) = fragment {
        out.push('#');
        out.push_str(fragment);
    }
    out
}

/// Percent/`+`-decode a query key for classification only. The raw key is kept
/// verbatim in the output; this decoded form is used solely to run the
/// sensitivity classifier against the real name.
fn form_decode_key(key_raw: &str) -> String {
    url::form_urlencoded::parse(key_raw.as_bytes())
        .next()
        .map_or_else(String::new, |(key, _)| key.into_owned())
}

/// Apply [`TapeRules`] to a recorded response: drop headers, apply the body
/// policy. Header dropping is case-insensitive over
/// [`BASE_DROPPED_RESPONSE_HEADERS`] plus `rules.drop_response_headers`.
pub fn rewrite_response(
    rules: &TapeRules,
    response: RecordedResponse,
) -> Result<RewrittenResponse, ScrubError> {
    match response {
        RecordedResponse::Http {
            status,
            headers,
            body,
        } => Ok(RewrittenResponse::Http {
            status,
            headers: drop_headers(rules, headers),
            body: apply_body_policy(&rules.body, body)?,
        }),
        RecordedResponse::Blob {
            status,
            content_type,
            etag,
            response_headers,
            body,
        } => Ok(RewrittenResponse::Blob {
            status,
            content_type,
            etag,
            response_headers: drop_headers(rules, response_headers),
            body: apply_body_policy(&rules.body, body)?,
        }),
        // Errors carry no headers or body to rewrite.
        RecordedResponse::Error {
            kind,
            message,
            retryable,
        } => Ok(RewrittenResponse::Error {
            kind,
            message,
            retryable,
        }),
    }
}

fn drop_headers(rules: &TapeRules, headers: Vec<TapeHeader>) -> Vec<TapeHeader> {
    headers
        .into_iter()
        .filter(|header| !is_dropped(rules, &header.name))
        .collect()
}

fn is_dropped(rules: &TapeRules, name: &str) -> bool {
    BASE_DROPPED_RESPONSE_HEADERS
        .iter()
        .chain(rules.drop_response_headers.iter())
        .any(|dropped| dropped.eq_ignore_ascii_case(name))
}

fn apply_body_policy(policy: &BodyPolicy, body: Vec<u8>) -> Result<Vec<u8>, ScrubError> {
    match policy {
        BodyPolicy::Verbatim => Ok(body),
        BodyPolicy::RewrittenJson {
            sanitize_fields,
            normalize_fields,
        } => rewrite_json(&body, sanitize_fields, normalize_fields),
    }
}

/// Parse `body` as `JSON`, replace `sanitize_fields` values with a deterministic
/// per-value token and `normalize_fields` values with `<volatile>`, then
/// re-serialize with `serde_json::to_string_pretty`.
fn rewrite_json(
    body: &[u8],
    sanitize_fields: &[&str],
    normalize_fields: &[&str],
) -> Result<Vec<u8>, ScrubError> {
    let mut value: Value =
        serde_json::from_slice(body).map_err(|source| ScrubError::NonJsonBody { source })?;
    rewrite_value(&mut value, sanitize_fields, normalize_fields);
    // Serializing a Value that just parsed never fails.
    let pretty = serde_json::to_string_pretty(&value)
        .map_err(|source| ScrubError::NonJsonBody { source })?;
    Ok(pretty.into_bytes())
}

fn rewrite_value(value: &mut Value, sanitize: &[&str], normalize: &[&str]) {
    match value {
        Value::Object(map) => {
            for (key, field) in map.iter_mut() {
                if sanitize.contains(&key.as_str()) {
                    *field = Value::String(sanitize_token(field));
                } else if normalize.contains(&key.as_str()) {
                    *field = Value::String(VOLATILE.to_owned());
                } else {
                    // A matched field's value is replaced wholesale; only
                    // unmatched values are descended into.
                    rewrite_value(field, sanitize, normalize);
                }
            }
        },
        Value::Array(items) => {
            for item in items {
                rewrite_value(item, sanitize, normalize);
            }
        },
        _ => {},
    }
}

/// `<redacted:{first 8 hex of sha256 of the serialized value}>`. Deterministic
/// per value (same value yields the same token) yet non-reversible.
fn sanitize_token(value: &Value) -> String {
    let serialized = serde_json::to_string(value).expect("serializing a json value never fails");
    let hex = sha256_hex(serialized.as_bytes());
    format!("<redacted:{}>", &hex[..8])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(name: &str, value: &str) -> Header {
        Header {
            name: name.to_owned(),
            value: value.to_owned(),
        }
    }

    #[test]
    fn sensitive_headers_scrubbed_names_and_order_kept() {
        let headers = [
            header("Accept", "application/json"),
            header("Authorization", "Bearer secret-token"),
            header("X-Api-Key", "abc123"),
            header("User-Agent", "omnifs"),
        ];
        let scrubbed = scrub_request("GET", "https://example.com/x", &headers, None);
        let names: Vec<_> = scrubbed.headers.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(
            names,
            ["Accept", "Authorization", "X-Api-Key", "User-Agent"]
        );
        assert_eq!(scrubbed.headers[0].value, "application/json");
        assert_eq!(scrubbed.headers[1].value, SCRUBBED);
        assert_eq!(scrubbed.headers[2].value, SCRUBBED);
        assert_eq!(scrubbed.headers[3].value, "omnifs");
    }

    #[test]
    fn sensitive_query_params_scrubbed_order_preserved() {
        let scrubbed = scrub_request(
            "GET",
            "https://api.example.com/data?page=2&access_token=deadbeef&ref=main",
            &[],
            None,
        );
        assert!(scrubbed.url.contains("page=2"));
        assert!(scrubbed.url.contains("ref=main"));
        assert!(scrubbed.url.contains(&format!("access_token={SCRUBBED}")));
        assert!(!scrubbed.url.contains("deadbeef"));
        // Order preserved: page before access_token before ref.
        let page = scrubbed.url.find("page").expect("page");
        let token = scrubbed.url.find("access_token").expect("token");
        let reff = scrubbed.url.find("ref").expect("ref");
        assert!(page < token && token < reff);
    }

    #[test]
    fn non_sensitive_url_passes_through() {
        let scrubbed = scrub_request("GET", "https://example.com/a?page=1&per_page=30", &[], None);
        assert!(scrubbed.url.contains("page=1"));
        assert!(scrubbed.url.contains("per_page=30"));
    }

    #[test]
    #[should_panic(expected = "unparseable URL")]
    fn unparseable_url_is_a_hard_error() {
        let _ = scrub_request("GET", "not a url", &[], None);
    }

    #[test]
    fn body_sha256_present_only_when_non_empty() {
        assert!(
            scrub_request("POST", "https://example.com", &[], None)
                .body_sha256
                .is_none()
        );
        assert!(
            scrub_request("POST", "https://example.com", &[], Some(b""))
                .body_sha256
                .is_none()
        );
        let with_body = scrub_request("POST", "https://example.com", &[], Some(b"payload"));
        assert_eq!(
            with_body.body_sha256.as_deref(),
            Some(sha256_hex(b"payload").as_str())
        );
    }

    #[test]
    fn dropped_headers_are_case_insensitive() {
        let rules = TapeRules {
            drop_response_headers: &["X-Custom-Volatile"],
            body: BodyPolicy::Verbatim,
        };
        let response = RecordedResponse::Http {
            status: 200,
            headers: vec![
                TapeHeader {
                    name: "Date".into(),
                    value: "now".into(),
                },
                TapeHeader {
                    name: "ETag".into(),
                    value: "\"v1\"".into(),
                },
                TapeHeader {
                    name: "CF-RAY".into(),
                    value: "x".into(),
                },
                TapeHeader {
                    name: "x-custom-volatile".into(),
                    value: "y".into(),
                },
                TapeHeader {
                    name: "Content-Type".into(),
                    value: "application/json".into(),
                },
            ],
            body: b"{}".to_vec(),
        };
        let RewrittenResponse::Http { headers, .. } =
            rewrite_response(&rules, response).expect("rewrite")
        else {
            panic!("expected http");
        };
        let kept: Vec<_> = headers.iter().map(|h| h.name.as_str()).collect();
        // Date (base, lowercase-match), CF-RAY (base, mixed case), and the
        // per-provider volatile header are all dropped; etag is preserved.
        assert_eq!(kept, ["ETag", "Content-Type"]);
    }

    #[test]
    fn verbatim_body_is_unchanged() {
        let rules = TapeRules::default();
        let response = RecordedResponse::Http {
            status: 200,
            headers: vec![],
            body: b"raw upstream bytes\n\t{".to_vec(),
        };
        let RewrittenResponse::Http { body, .. } =
            rewrite_response(&rules, response).expect("rewrite")
        else {
            panic!("expected http");
        };
        assert_eq!(body, b"raw upstream bytes\n\t{");
    }

    #[test]
    fn rewritten_json_sanitizes_and_normalizes_at_depth() {
        let rules = TapeRules {
            drop_response_headers: &[],
            body: BodyPolicy::RewrittenJson {
                sanitize_fields: &["hr"],
                normalize_fields: &["timestamp"],
            },
        };
        let body = br#"{"timestamp":"2026-07-07T00:00:00Z","data":[{"hr":72,"steps":1000}]}"#;
        let response = RecordedResponse::Http {
            status: 200,
            headers: vec![],
            body: body.to_vec(),
        };
        let RewrittenResponse::Http { body, .. } =
            rewrite_response(&rules, response).expect("rewrite")
        else {
            panic!("expected http");
        };
        let parsed: Value = serde_json::from_slice(&body).expect("valid json out");
        assert_eq!(parsed["timestamp"], Value::String(VOLATILE.into()));
        // Nested sanitize field is redacted; sibling non-matched field survives.
        let hr = parsed["data"][0]["hr"]
            .as_str()
            .expect("hr redacted to string");
        assert!(hr.starts_with("<redacted:") && hr.ends_with('>'));
        assert_eq!(parsed["data"][0]["steps"], Value::from(1000));
    }

    #[test]
    fn sanitize_token_is_deterministic_and_distinct() {
        assert_eq!(
            sanitize_token(&Value::from(72)),
            sanitize_token(&Value::from(72))
        );
        assert_ne!(
            sanitize_token(&Value::from(72)),
            sanitize_token(&Value::from(73))
        );
        // Token shape: <redacted:XXXXXXXX> with 8 hex chars.
        let token = sanitize_token(&Value::from(72));
        assert!(token.starts_with("<redacted:"));
        assert_eq!(token.len(), "<redacted:>".len() + 8);
    }

    #[test]
    fn rewritten_json_rejects_non_json() {
        let rules = TapeRules {
            drop_response_headers: &[],
            body: BodyPolicy::RewrittenJson {
                sanitize_fields: &[],
                normalize_fields: &[],
            },
        };
        let response = RecordedResponse::Http {
            status: 200,
            headers: vec![],
            body: b"<html>not json</html>".to_vec(),
        };
        assert!(matches!(
            rewrite_response(&rules, response),
            Err(ScrubError::NonJsonBody { .. })
        ));
    }
}
