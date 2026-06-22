//! Inspector-stream redaction (stricter than daemon debug tracing).

use std::fmt::{self, Write as _};

/// Inspector summaries strip the entire query string unless the name is allowlisted.
const ALLOWLISTED_QUERY_KEYS: &[&str] = &[];

pub fn is_sensitive_header(name: &str) -> bool {
    const SENSITIVE: &[&str] = &[
        "authorization",
        "proxy-authorization",
        "cookie",
        "set-cookie",
        "x-api-key",
        "x-github-token",
        "x-auth-token",
    ];
    SENSITIVE
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(name))
        || {
            let lower = name.to_ascii_lowercase();
            lower.contains("token") || lower.contains("secret") || lower.contains("password")
        }
}

pub fn is_sensitive_query_param(name: &str) -> bool {
    if ALLOWLISTED_QUERY_KEYS
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(name))
    {
        return false;
    }
    let name = name.to_ascii_lowercase();
    name.contains("token")
        || name.contains("secret")
        || name.contains("password")
        || name == "key"
        || name.ends_with("_key")
        || name == "access_token"
}

/// Redact a URL for the inspector stream: no credentials, no query unless allowlisted.
pub fn redact_url_for_live(raw: &str) -> String {
    let Ok(mut parsed) = url::Url::parse(raw) else {
        return raw.to_string();
    };

    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);

    if parsed.query().is_some() {
        let allowlisted = parsed
            .query_pairs()
            .filter(|(name, _)| {
                ALLOWLISTED_QUERY_KEYS
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(name))
            })
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect::<Vec<_>>();
        parsed.set_query(None);
        if !allowlisted.is_empty() {
            let mut pairs = parsed.query_pairs_mut();
            for (name, value) in allowlisted {
                pairs.append_pair(&name, &value);
            }
        }
    }

    parsed.to_string()
}

/// Build a compact HTTP callout summary: `GET host/path` without query secrets.
pub fn redact_http_url_for_summary(method: &str, raw_url: &str) -> String {
    let redacted = redact_url_for_live(raw_url);
    let Ok(parsed) = url::Url::parse(&redacted) else {
        return format!("{method} {raw_url}");
    };
    let host = parsed.host_str().unwrap_or("unknown");
    let path = parsed.path();
    format!("{method} {host}{path}")
}

/// Normalize `git@github.com:owner/repo.git` or https remotes for display.
pub fn redact_git_remote(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("git@") {
        let (host, path) = rest.split_once(':').unwrap_or((rest, ""));
        let path = path.trim_end_matches(".git");
        return format!("{host}:{path}");
    }
    if let Ok(parsed) = url::Url::parse(trimmed) {
        let host = parsed.host_str().unwrap_or("unknown");
        let path = parsed
            .path()
            .trim_start_matches('/')
            .trim_end_matches(".git");
        return format!("{host}:{path}");
    }
    trimmed.to_string()
}

/// Reject summaries that still look like raw upstream URLs for blob/cache-key rules.
pub fn summary_is_cache_key_shaped(summary: &str) -> bool {
    !summary.contains("://") && !summary.contains('?') && summary.len() <= 512
}

pub fn write_truncated(f: &mut fmt::Formatter<'_>, value: &str, max: usize) -> fmt::Result {
    for (index, ch) in value.chars().enumerate() {
        if index == max {
            f.write_str("...")?;
            return Ok(());
        }
        f.write_char(ch)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── is_sensitive_header ───────────────────────────────────────────────

    #[test]
    fn sensitive_header_classification() {
        assert!(is_sensitive_header("Authorization"));
        assert!(is_sensitive_header("AUTHORIZATION"));
        assert!(is_sensitive_header("authorization"));
        assert!(is_sensitive_header("Proxy-Authorization"));
        assert!(is_sensitive_header("Cookie"));
        assert!(is_sensitive_header("Set-Cookie"));
        assert!(is_sensitive_header("X-Api-Key"));
        assert!(is_sensitive_header("X-GitHub-Token"));
        assert!(is_sensitive_header("X-Auth-Token"));
        assert!(is_sensitive_header("X-My-Token"));
        assert!(is_sensitive_header("X-ACCESS-TOKEN"));
        assert!(is_sensitive_header("My-Secret-Header"));
        assert!(is_sensitive_header("Password-Hint"));
        assert!(is_sensitive_header("x-client-secret"));
        assert!(!is_sensitive_header("User-Agent"));
        assert!(!is_sensitive_header("Content-Type"));
        assert!(!is_sensitive_header("Accept"));
        assert!(!is_sensitive_header("X-Request-Id"));
    }

    // ── is_sensitive_query_param ──────────────────────────────────────────

    #[test]
    fn sensitive_query_param_classification() {
        assert!(is_sensitive_query_param("access_token"));
        assert!(is_sensitive_query_param("client_secret"));
        assert!(is_sensitive_query_param("password"));
        assert!(is_sensitive_query_param("api_key"));
        assert!(is_sensitive_query_param("key"));
        assert!(is_sensitive_query_param("my_key"));
        assert!(is_sensitive_query_param("ACCESS_TOKEN"));
        assert!(is_sensitive_query_param("Client_Secret"));
        assert!(is_sensitive_query_param("PASSWORD"));
        assert!(!is_sensitive_query_param("ref"));
        assert!(!is_sensitive_query_param("page"));
        assert!(!is_sensitive_query_param("per_page"));
        assert!(!is_sensitive_query_param("format"));
    }

    // ── redact_url_for_live ───────────────────────────────────────────────

    #[test]
    fn live_url_strips_userinfo_and_query() {
        let out = redact_url_for_live(
            "https://user:pass@api.github.com/repos/o/r?access_token=secret&ref=main",
        );
        assert!(!out.contains("user"), "username leaked");
        assert!(!out.contains("pass"), "password leaked");
        assert!(!out.contains("access_token"), "secret param name leaked");
        assert!(!out.contains("secret"), "secret value leaked");
        assert!(!out.contains('?'), "query separator leaked");
        assert!(out.contains("api.github.com/repos/o/r"));
    }

    #[test]
    fn live_url_strips_credentials_even_without_query() {
        let out = redact_url_for_live("https://token:x@api.github.com/data");
        assert!(!out.contains("token:x"), "credentials leaked");
        assert!(out.contains("api.github.com/data"));
    }

    #[test]
    fn live_url_passes_through_unparseable_input_unchanged() {
        // Not a valid URL: returned as-is rather than panicking or corrupting.
        let raw = "not a url at all";
        assert_eq!(redact_url_for_live(raw), raw);
    }

    // ── redact_http_url_for_summary ───────────────────────────────────────

    #[test]
    fn http_summary_uses_method_host_path() {
        let summary = redact_http_url_for_summary(
            "GET",
            "https://api.github.com/repos/raulk/omnifs?access_token=secret",
        );
        assert_eq!(summary, "GET api.github.com/repos/raulk/omnifs");
    }

    #[test]
    fn http_summary_falls_back_for_unparseable_url() {
        let raw = "not-a-url";
        let summary = redact_http_url_for_summary("POST", raw);
        // Must not panic; must include the method and original text.
        assert!(summary.starts_with("POST "));
        assert!(summary.contains(raw));
    }

    // ── redact_git_remote ─────────────────────────────────────────────────

    #[test]
    fn git_remote_redaction() {
        assert_eq!(
            redact_git_remote("git@github.com:raulk/omnifs.git"),
            "github.com:raulk/omnifs"
        );
        assert_eq!(
            redact_git_remote("git@github.com:raulk/omnifs"),
            "github.com:raulk/omnifs"
        );
        let out = redact_git_remote("https://user:token@github.com/org/repo.git");
        assert!(!out.contains("user"), "username leaked in https remote");
        assert!(!out.contains("token"), "token leaked in https remote");
        assert!(out.contains("github.com"), "host missing");
        assert!(out.contains("org/repo"), "path missing");
        assert_eq!(
            redact_git_remote("https://github.com/org/repo.git"),
            "github.com:org/repo"
        );
        assert_eq!(redact_git_remote("  somepath  "), "somepath");
    }

    // ── summary_is_cache_key_shaped ───────────────────────────────────────

    #[test]
    fn cache_key_summary_validation() {
        for summary in [
            "arxiv/pdf/2401.12345",
            "owner/repo/commit/abc123",
            "", // empty is technically fine
            &"x".repeat(512),
        ] {
            assert!(summary_is_cache_key_shaped(summary), "accept: {summary:?}");
        }

        for summary in [
            "https://example.com/x",
            "http://example.com/y",
            "owner/repo?ref=main",
            &"x".repeat(513),
        ] {
            assert!(!summary_is_cache_key_shaped(summary), "reject: {summary:?}");
        }
    }

    // ── write_truncated ───────────────────────────────────────────────────

    fn truncated(value: &str, max: usize) -> String {
        struct T<'a>(&'a str, usize);
        impl std::fmt::Display for T<'_> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write_truncated(f, self.0, self.1)
            }
        }
        T(value, max).to_string()
    }

    #[test]
    fn write_truncated_limits_output() {
        for (value, max, expected) in [
            ("hello", 10, "hello"),
            ("hello", 5, "hello"),
            ("hello!", 5, "hello..."),
            ("abcdefghij", 3, "abc..."),
        ] {
            let out = truncated(value, max);
            assert_eq!(out, expected, "value={value:?} max={max}");
            if max < value.len() {
                assert!(
                    !out.contains(value.chars().nth(max).unwrap()),
                    "leaked char past cutoff: {out}"
                );
            }
        }
    }
}
