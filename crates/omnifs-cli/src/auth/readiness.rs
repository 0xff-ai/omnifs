//! Auth readiness state for configured mounts.

use std::fmt::Write as _;

use omnifs_creds::{CredentialEntry, CredentialStore};
use omnifs_host::mounts::Resolved;

use crate::credential_target::CredentialTarget;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AuthReadiness {
    None,
    Ready {
        kind: String,
        scopes: Vec<String>,
        expires_at: Option<String>,
    },
    Missing {
        command: String,
    },
    ConfiguredExternally {
        source: String,
    },
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AuthProbeSeverity {
    Ok,
    Warn,
    Err,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthProbeSummary {
    pub(crate) severity: AuthProbeSeverity,
    pub(crate) message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AuthTerminalKind {
    None,
    Ready,
    External,
    Missing,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthTerminalRow {
    pub(crate) kind: AuthTerminalKind,
    pub(crate) summary: String,
}

impl AuthReadiness {
    pub(crate) fn from_config(config: &Resolved, store: &dyn CredentialStore) -> Self {
        let target = match CredentialTarget::from_resolved_mount(config) {
            Ok(target) => target,
            Err(error) => return Self::Error(error.to_string()),
        };

        match target {
            CredentialTarget::None => return Self::None,
            CredentialTarget::External(source) => {
                return Self::ConfiguredExternally {
                    source: source.to_string(),
                };
            },
            CredentialTarget::Internal(_) => {},
        }

        match target.lookup(store) {
            Ok(Some(entry)) => Self::from_entry(entry),
            Ok(None) => Self::Missing {
                command: format!("omnifs auth login {}", config.mount),
            },
            Err(error) => Self::Error(error.to_string()),
        }
    }

    pub(crate) fn from_entry(entry: CredentialEntry) -> Self {
        let expires_at = entry.expires_at().map(format_rfc3339);
        let kind = entry.kind().to_string();
        Self::Ready {
            kind,
            scopes: entry.into_scopes(),
            expires_at,
        }
    }

    pub(crate) fn probe_summary(&self) -> AuthProbeSummary {
        match self {
            Self::None => AuthProbeSummary {
                severity: AuthProbeSeverity::Ok,
                message: "no auth required".into(),
            },
            Self::Ready { kind, scopes, .. } => {
                let scopes_str = if scopes.is_empty() {
                    String::new()
                } else {
                    format!(" scopes={}", scopes.join(","))
                };
                AuthProbeSummary {
                    severity: AuthProbeSeverity::Ok,
                    message: format!("{kind}{scopes_str}"),
                }
            },
            Self::ConfiguredExternally { source } => AuthProbeSummary {
                severity: AuthProbeSeverity::Ok,
                message: format!("external ({source})"),
            },
            Self::Missing { command } => AuthProbeSummary {
                severity: AuthProbeSeverity::Warn,
                message: format!("run `{command}`"),
            },
            Self::Error(error) => AuthProbeSummary {
                severity: AuthProbeSeverity::Err,
                message: error.clone(),
            },
        }
    }

    pub(crate) fn terminal_row(&self) -> AuthTerminalRow {
        match self {
            Self::None => AuthTerminalRow {
                kind: AuthTerminalKind::None,
                summary: "no auth required".into(),
            },
            Self::Ready {
                kind,
                scopes,
                expires_at,
            } => {
                let mut summary = kind.clone();
                if !scopes.is_empty() {
                    let _ = write!(&mut summary, "  {} scopes", scopes.len());
                }
                if let Some(expires_at) = expires_at {
                    let _ = write!(&mut summary, "  expires {expires_at}");
                }
                AuthTerminalRow {
                    kind: AuthTerminalKind::Ready,
                    summary,
                }
            },
            Self::ConfiguredExternally { source } => AuthTerminalRow {
                kind: AuthTerminalKind::External,
                summary: format!("external ({source})"),
            },
            Self::Missing { command } => AuthTerminalRow {
                kind: AuthTerminalKind::Missing,
                summary: format!("missing — run `{command}`"),
            },
            Self::Error(error) => AuthTerminalRow {
                kind: AuthTerminalKind::Error,
                summary: format!("error: {error}"),
            },
        }
    }
}

fn format_rfc3339(value: time::OffsetDateTime) -> String {
    value
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_core::CredentialId;
    use omnifs_creds::MemoryStore;
    use omnifs_host::mounts::Spec;
    use secrecy::SecretString;
    use time::OffsetDateTime;

    fn resolved_mount(json: &str) -> Resolved {
        Spec::parse(json)
            .unwrap()
            .into_resolved("github", None)
            .unwrap()
    }

    #[test]
    fn from_config_reports_no_auth_required() {
        let config = resolved_mount(
            r#"{
                "provider": "omnifs_provider_github.wasm",
                "mount": "github"
            }"#,
        );
        let store = MemoryStore::new();
        assert_eq!(
            AuthReadiness::from_config(&config, &store),
            AuthReadiness::None
        );
    }

    #[test]
    fn from_config_reports_external_token_source() {
        let config = resolved_mount(
            r#"{
                "provider": "omnifs_provider_github.wasm",
                "mount": "github",
                "auth": [{ "type": "static-token", "token_env": "GITHUB_TOKEN" }]
            }"#,
        );
        let store = MemoryStore::new();
        assert_eq!(
            AuthReadiness::from_config(&config, &store),
            AuthReadiness::ConfiguredExternally {
                source: "token_env=GITHUB_TOKEN".into(),
            }
        );
    }

    #[test]
    fn from_config_reports_ready_credential() {
        let config = resolved_mount(
            r#"{
                "provider": "omnifs_provider_github.wasm",
                "mount": "github",
                "auth": [{ "type": "oauth", "scheme": "device" }]
            }"#,
        );
        let store = MemoryStore::new();
        let key = CredentialId::new("github", "device", "default").unwrap();
        store
            .put(
                &key,
                &CredentialEntry::oauth(
                    SecretString::from("token".to_owned()),
                    None,
                    None,
                    "bearer".to_owned(),
                    vec!["repo".to_owned()],
                    OffsetDateTime::UNIX_EPOCH,
                ),
            )
            .unwrap();
        match AuthReadiness::from_config(&config, &store) {
            AuthReadiness::Ready { kind, scopes, .. } => {
                assert_eq!(kind, "oauth");
                assert_eq!(scopes, vec!["repo".to_string()]);
            },
            other => panic!("expected ready auth, got {other:?}"),
        }
    }

    #[test]
    fn from_config_reports_missing_credential() {
        let config = resolved_mount(
            r#"{
                "provider": "omnifs_provider_github.wasm",
                "mount": "github",
                "auth": [{ "type": "oauth", "scheme": "device" }]
            }"#,
        );
        let store = MemoryStore::new();
        assert_eq!(
            AuthReadiness::from_config(&config, &store),
            AuthReadiness::Missing {
                command: "omnifs auth login github".into(),
            }
        );
    }

    #[test]
    fn probe_and_terminal_projections_cover_error_state() {
        let auth = AuthReadiness::Error("boom".into());
        assert_eq!(
            auth.probe_summary(),
            AuthProbeSummary {
                severity: AuthProbeSeverity::Err,
                message: "boom".into(),
            }
        );
        assert_eq!(
            auth.terminal_row(),
            AuthTerminalRow {
                kind: AuthTerminalKind::Error,
                summary: "error: boom".into(),
            }
        );
    }
}
