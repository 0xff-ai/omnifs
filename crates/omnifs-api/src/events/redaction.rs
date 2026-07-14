//! Inspector-stream redaction (stricter than daemon debug tracing).

use std::fmt::{self, Write as _};

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
    let name = name.to_ascii_lowercase();
    name.contains("token")
        || name.contains("secret")
        || name.contains("password")
        || name == "key"
        || name.ends_with("_key")
}

/// Build a compact HTTP callout summary: `GET host/path` without query secrets.
pub fn redact_http_url_for_summary(method: &str, raw_url: &str) -> String {
    let Ok(parsed) = url::Url::parse(raw_url) else {
        return format!("{method} <redacted-url>");
    };
    let Some(host) = parsed.host_str() else {
        return format!("{method} <redacted-url>");
    };
    let path = parsed.path();
    format!("{method} {host}{path}")
}

/// Normalize `git@github.com:owner/repo.git` or https remotes for display.
pub fn redact_git_remote(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("git@") {
        let Some((host, path)) = rest.split_once(':') else {
            return "<redacted-git-remote>".to_string();
        };
        if host.is_empty() || path.is_empty() {
            return "<redacted-git-remote>".to_string();
        }
        let path = path.trim_end_matches(".git");
        return format!("{host}:{path}");
    }
    if let Ok(parsed) = url::Url::parse(trimmed) {
        let Some(host) = parsed.host_str() else {
            return "<redacted-git-remote>".to_string();
        };
        let path = parsed
            .path()
            .trim_start_matches('/')
            .trim_end_matches(".git");
        return format!("{host}:{path}");
    }
    "<redacted-git-remote>".to_string()
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

    // ── redact_http_url_for_summary ───────────────────────────────────────

    #[test]
    fn http_summary_uses_method_host_path() {
        let summary = redact_http_url_for_summary(
            "GET",
            "https://user:pass@api.github.com/repos/raulk/omnifs?access_token=secret",
        );
        assert_eq!(summary, "GET api.github.com/repos/raulk/omnifs");
    }

    #[test]
    fn http_summary_falls_back_for_unparseable_url() {
        let raw = "not-a-url";
        let summary = redact_http_url_for_summary("POST", raw);
        // Malformed input must stay opaque because spans are created before validation.
        assert!(summary.starts_with("POST "));
        assert!(!summary.contains(raw));
        assert_eq!(summary, "POST <redacted-url>");
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
        assert_eq!(redact_git_remote("  somepath  "), "<redacted-git-remote>");
        assert_eq!(
            redact_git_remote("https://user:secret@["),
            "<redacted-git-remote>"
        );
        assert_eq!(redact_git_remote("git@malformed"), "<redacted-git-remote>");
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
