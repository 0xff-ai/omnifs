//! Auth readiness state for configured mounts.

use omnifs_workspace::authn::AuthKind;
use omnifs_workspace::authn::CredentialId;
use omnifs_workspace::creds::{CredentialEntry, CredentialStore, Refreshability};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
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
    Error {
        message: String,
    },
}

impl AuthReadiness {
    pub(crate) fn from_credential(
        mount_name: &str,
        credential_id: Option<&CredentialId>,
        store: &dyn CredentialStore,
    ) -> Self {
        let Some(credential_id) = credential_id else {
            return Self::None;
        };

        let command = format!("omnifs mount reauth {mount_name}");
        match store.get(credential_id) {
            Ok(Some(entry)) => Self::from_entry(entry, Some(&command)),
            Ok(None) => Self::Missing { command },
            Err(error) => Self::Error {
                message: error.to_string(),
            },
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
        let command = reauth_command.unwrap_or("omnifs mount reauth <mount>");
        return vec![format!("expired; run `{command}`")];
    }
    vec!["not refreshable; re-authentication will be required after expiry".to_owned()]
}

pub(crate) fn format_rfc3339(value: time::OffsetDateTime) -> String {
    value
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_workspace::authn::CredentialId;
    use omnifs_workspace::creds::MemoryStore;
    use secrecy::SecretString;
    use time::OffsetDateTime;

    #[test]
    fn from_credential_reports_ready_credential() {
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
        match AuthReadiness::from_credential("github", Some(&key), &store) {
            AuthReadiness::Ready { kind, scopes, .. } => {
                assert_eq!(kind, "oauth");
                assert_eq!(scopes, vec!["repo".to_string()]);
            },
            other => panic!("expected ready auth, got {other:?}"),
        }
    }

    #[test]
    fn from_credential_reports_missing_credential() {
        let store = MemoryStore::new();
        let key = CredentialId::new("github", "device", "default").unwrap();
        assert_eq!(
            AuthReadiness::from_credential("github", Some(&key), &store),
            AuthReadiness::Missing {
                command: "omnifs mount reauth github".into(),
            }
        );
    }
}
