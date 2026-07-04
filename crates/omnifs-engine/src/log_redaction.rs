use omnifs_wit::provider::types as wit_types;
use std::fmt::{self, Write as _};

pub(crate) struct LogUrl<'a>(pub(crate) &'a str);

impl fmt::Display for LogUrl<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Ok(mut parsed) = url::Url::parse(self.0) else {
            return f.write_str(self.0);
        };

        let _ = parsed.set_username("");
        let _ = parsed.set_password(None);

        let query_pairs = parsed.query_pairs().into_owned().collect::<Vec<_>>();
        if !query_pairs.is_empty() {
            parsed.set_query(None);
            {
                let mut pairs = parsed.query_pairs_mut();
                for (name, value) in query_pairs {
                    let logged_value = if is_sensitive_query_param(&name) {
                        "redacted"
                    } else {
                        value.as_str()
                    };
                    pairs.append_pair(&name, logged_value);
                }
            }
        }

        write!(f, "{parsed}")
    }
}

pub(crate) struct WitHeaders<'a>(pub(crate) &'a [wit_types::Header]);

impl fmt::Display for WitHeaders<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, header) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_char(',')?;
            }
            write!(f, "{}=", header.name)?;
            if is_sensitive_header(&header.name) {
                f.write_str("<redacted>")?;
            } else {
                write_truncated_for_log(f, &header.value, 256)?;
            }
        }
        Ok(())
    }
}

pub(crate) fn write_truncated_for_log(
    f: &mut fmt::Formatter<'_>,
    value: &str,
    max: usize,
) -> fmt::Result {
    for (index, ch) in value.chars().enumerate() {
        if index == max {
            f.write_str("...")?;
            return Ok(());
        }
        f.write_char(ch)?;
    }
    Ok(())
}

pub(crate) fn is_sensitive_header(name: &str) -> bool {
    const SENSITIVE: &[&str] = &[
        "authorization",
        "proxy-authorization",
        "cookie",
        "set-cookie",
        "x-api-key",
    ];
    SENSITIVE
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(name))
}

pub(crate) fn is_sensitive_query_param(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.contains("token")
        || name.contains("secret")
        || name.contains("password")
        || name == "key"
        || name.ends_with("_key")
}

#[cfg(test)]
mod callout_log_tests {
    use super::*;
    use omnifs_wit::provider::types as wit_types;

    #[test]
    fn url_for_log_preserves_diagnostic_query_and_redacts_secrets() {
        let logged = LogUrl(
            "https://user:pass@example.com/api?search_query=cat%3Acs.AI&access_token=secret",
        )
        .to_string();

        assert!(logged.contains("search_query=cat%3Acs.AI"));
        assert!(logged.contains("access_token=redacted"));
        assert!(!logged.contains("user:pass"));
        assert!(!logged.contains("secret"));
    }

    #[test]
    fn headers_for_log_redacts_credentials() {
        let logged = WitHeaders(&[
            wit_types::Header {
                name: "User-Agent".to_string(),
                value: "omnifs-provider-arxiv/0.1.0".to_string(),
            },
            wit_types::Header {
                name: "Authorization".to_string(),
                value: "Bearer secret".to_string(),
            },
        ])
        .to_string();

        assert!(logged.contains("User-Agent=omnifs-provider-arxiv/0.1.0"));
        assert!(logged.contains("Authorization=<redacted>"));
        assert!(!logged.contains("Bearer secret"));
    }
}
