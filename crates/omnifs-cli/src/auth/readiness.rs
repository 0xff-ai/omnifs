//! Auth readiness state for configured mounts.

use std::fmt::Write as _;

use omnifs_core::AuthKind;
use omnifs_creds::{CredentialEntry, CredentialStore, Refreshability};
use omnifs_mount::mounts::Resolved;

use crate::credential_target::CredentialTarget;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AuthReadiness {
    None,
    Ready {
        kind: String,
        scopes: Vec<String>,
        expires_at: Option<String>,
        refreshability: Refreshability,
        notices: Vec<String>,
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

        let command = format!("omnifs auth login {}", config.spec.mount);
        match target.lookup(store) {
            Ok(Some(entry)) => Self::from_entry(entry, Some(&command)),
            Ok(None) => Self::Missing { command },
            Err(error) => Self::Error(error.to_string()),
        }
    }

    pub(crate) fn from_entry(entry: CredentialEntry, reauth_command: Option<&str>) -> Self {
        let expires_at = entry.expires_at().map(format_rfc3339);
        let kind = entry.kind().to_string();
        let refreshability = entry.refreshability();
        let notices = credential_notices(&entry, reauth_command);
        Self::Ready {
            kind,
            scopes: entry.into_scopes(),
            expires_at,
            refreshability,
            notices,
        }
    }

    pub(crate) fn probe_summary(&self) -> AuthProbeSummary {
        match self {
            Self::None => AuthProbeSummary {
                severity: AuthProbeSeverity::Ok,
                message: "no auth required".into(),
            },
            Self::Ready {
                kind,
                scopes,
                notices,
                ..
            } => {
                let scopes_str = if scopes.is_empty() {
                    String::new()
                } else {
                    format!(" scopes={}", scopes.join(","))
                };
                let notice_str = if notices.is_empty() {
                    String::new()
                } else {
                    format!(" {}", notices.join("; "))
                };
                AuthProbeSummary {
                    severity: if notices.is_empty() {
                        AuthProbeSeverity::Ok
                    } else {
                        AuthProbeSeverity::Warn
                    },
                    message: format!("{kind}{scopes_str}{notice_str}"),
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
                refreshability,
                notices,
            } => {
                let mut summary = kind.clone();
                if !scopes.is_empty() {
                    let _ = write!(&mut summary, "  {} scopes", scopes.len());
                }
                if let Some(expires_at) = expires_at {
                    let _ = write!(&mut summary, "  expires {expires_at}");
                }
                if *refreshability != Refreshability::NotApplicable {
                    let _ = write!(&mut summary, "  {refreshability}");
                }
                if !notices.is_empty() {
                    let _ = write!(&mut summary, "  {}", notices.join("; "));
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

pub(crate) fn credential_notices(
    entry: &CredentialEntry,
    reauth_command: Option<&str>,
) -> Vec<String> {
    if entry.kind() != AuthKind::OAuth || entry.refreshability() != Refreshability::NotRefreshable {
        return Vec::new();
    }
    if entry.expires_at().is_none() {
        return Vec::new();
    }
    if entry.is_expired_at(time::OffsetDateTime::now_utc()) {
        let command = reauth_command.unwrap_or("omnifs auth login <mount>");
        return vec![format!("expired; run `{command}`")];
    }
    vec!["not refreshable; re-authentication will be required after expiry".to_owned()]
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
    use omnifs_mount::mounts::Spec;
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
