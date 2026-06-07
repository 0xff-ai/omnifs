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

    #[test]
    fn live_url_strips_query_and_credentials() {
        let logged = redact_url_for_live(
            "https://user:pass@api.github.com/repos/o/r?access_token=secret&ref=main",
        );
        assert!(!logged.contains("user:pass"));
        assert!(!logged.contains("access_token"));
        assert!(!logged.contains("secret"));
        assert!(!logged.contains('?'));
        assert!(logged.contains("api.github.com/repos/o/r"));
    }

    #[test]
    fn http_summary_uses_method_host_path() {
        let summary = redact_http_url_for_summary(
            "GET",
            "https://api.github.com/repos/raulk/omnifs?access_token=secret",
        );
        assert_eq!(summary, "GET api.github.com/repos/raulk/omnifs");
    }

    #[test]
    fn git_remote_redacts_ssh_form() {
        assert_eq!(
            redact_git_remote("git@github.com:raulk/omnifs.git"),
            "github.com:raulk/omnifs"
        );
    }

    #[test]
    fn sensitive_header_names_include_github_token() {
        assert!(is_sensitive_header("X-GitHub-Token"));
        assert!(!is_sensitive_header("User-Agent"));
    }

    #[test]
    fn cache_key_shaped_summary_rejects_urls() {
        assert!(summary_is_cache_key_shaped("arxiv/pdf/2401.12345"));
        assert!(!summary_is_cache_key_shaped("https://example.com/x"));
    }
}
