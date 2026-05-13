//! Shared header construction + response-header decoding helpers
//! used by the HTTP and blob executors.

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::str::FromStr;
use tracing::warn;

/// Combine auth and request headers into a single `HeaderMap`. Invalid
/// header names or values are reported with their source so the
/// caller's diagnostics distinguish a bad provider header from a bad
/// host-injected auth header.
pub(crate) fn build_header_map<'a, A, R>(
    auth_headers: A,
    request_headers: R,
) -> Result<HeaderMap, String>
where
    A: IntoIterator<Item = (&'a str, &'a str)>,
    R: IntoIterator<Item = (&'a str, &'a str)>,
{
    let mut header_map = HeaderMap::new();
    append_headers(&mut header_map, auth_headers, "auth")?;
    append_headers(&mut header_map, request_headers, "request")?;
    Ok(header_map)
}

fn append_headers<'a, I>(header_map: &mut HeaderMap, headers: I, source: &str) -> Result<(), String>
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    for (name, value) in headers {
        let header_name = HeaderName::from_str(name)
            .map_err(|error| format!("invalid {source} header name `{name}`: {error}"))?;
        let header_value = HeaderValue::from_str(value).map_err(|error| {
            format!(
                "invalid {source} header value for `{}`: {error}",
                header_name.as_str()
            )
        })?;
        header_map.append(header_name, header_value);
    }
    Ok(())
}

/// Decode a `HeaderMap` from reqwest into the WIT-friendly `(name,
/// value)` shape, dropping non-UTF8 values rather than failing the
/// whole response (provider headers are UTF-8 only by contract).
pub(crate) fn decode_response_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| match value.to_str() {
            Ok(value) => Some((name.as_str().to_string(), value.to_string())),
            Err(error) => {
                warn!(
                    header = %name,
                    err = %error,
                    "dropping non-UTF8 response header because provider headers are UTF-8 only"
                );
                None
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_header_map_rejects_invalid_header_name() {
        let error = build_header_map(
            std::iter::empty::<(&str, &str)>(),
            [("bad header", "value")],
        )
        .unwrap_err();
        assert!(error.contains("invalid request header name"));
    }

    #[test]
    fn decode_response_headers_drops_non_utf8_values() {
        let mut headers = HeaderMap::new();
        headers.insert("x-valid", HeaderValue::from_static("ok"));
        headers.insert("x-bytes", HeaderValue::from_bytes(b"\x80binary").unwrap());

        let response_headers = decode_response_headers(&headers);

        assert_eq!(
            response_headers,
            vec![("x-valid".to_string(), "ok".to_string())]
        );
    }
}
