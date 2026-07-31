use omnifs_core::{AuthRuntimeFingerprint, CredentialGeneration, CredentialVersion, ProviderId};
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, ZeroizeOnDrop};

/// Secret bytes accepted only on the local control socket.
#[derive(Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
#[serde(transparent)]
pub struct SecretBytes(Vec<u8>);

impl SecretBytes {
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for SecretBytes {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SecretBytes([REDACTED])")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialKey {
    pub provider_name: String,
    pub scheme: String,
    pub account_label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CredentialKind {
    StaticToken,
    OAuth,
}

#[derive(Serialize, Deserialize)]
pub enum CredentialMaterial {
    StaticToken {
        token: SecretBytes,
    },
    OAuth {
        access_token: SecretBytes,
        refresh_token: Option<SecretBytes>,
        expires_at_unix: Option<i64>,
        token_type: String,
        scopes: Vec<String>,
        upstream_identity: Option<String>,
    },
}

impl std::fmt::Debug for CredentialMaterial {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StaticToken { .. } => formatter
                .debug_struct("StaticToken")
                .field("token", &"[REDACTED]")
                .finish(),
            Self::OAuth {
                refresh_token,
                expires_at_unix,
                token_type,
                scopes,
                upstream_identity,
                ..
            } => formatter
                .debug_struct("OAuth")
                .field("access_token", &"[REDACTED]")
                .field(
                    "refresh_token",
                    &refresh_token.as_ref().map(|_| "[REDACTED]"),
                )
                .field("expires_at_unix", expires_at_unix)
                .field("token_type", token_type)
                .field("scopes", scopes)
                .field("upstream_identity", upstream_identity)
                .finish(),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialClientOverrides {
    pub client_id: Option<String>,
    pub client_secret: Option<SecretBytes>,
    pub redirect_uri: Option<String>,
    pub scopes: Option<Vec<String>>,
}

impl std::fmt::Debug for CredentialClientOverrides {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CredentialClientOverrides")
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "[REDACTED]"),
            )
            .field("redirect_uri", &self.redirect_uri)
            .field("scopes", &self.scopes)
            .finish()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialSubmission {
    pub provider: ProviderId,
    pub scheme: String,
    pub account_label: String,
    pub material: CredentialMaterial,
    pub overrides: CredentialClientOverrides,
}

impl std::fmt::Debug for CredentialSubmission {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CredentialSubmission")
            .field("provider", &self.provider)
            .field("scheme", &self.scheme)
            .field("account_label", &self.account_label)
            .field("material", &self.material)
            .field("overrides", &self.overrides)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CredentialStatusKind {
    Active,
    Blocked,
    PendingRepublish,
    RevocationPending,
    RevocationUnknown,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialStatus {
    pub key: CredentialKey,
    pub provider: ProviderId,
    pub kind: CredentialKind,
    /// Effective scopes granted by the upstream OAuth exchange.
    ///
    /// Static-token credentials always report an empty list. This field is
    /// non-secret and intentionally excludes the token material itself.
    pub scopes: Vec<String>,
    pub auth_fingerprint: AuthRuntimeFingerprint,
    pub version: CredentialVersion,
    pub generation: CredentialGeneration,
    /// Monotonic precondition for durable credential actions.
    pub action_generation: u64,
    pub status: CredentialStatusKind,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_submission_has_redacted_debug_output() {
        let submission = CredentialSubmission {
            provider: ProviderId::from_wasm_bytes(b"demo"),
            scheme: "oauth".to_owned(),
            account_label: "default".to_owned(),
            material: CredentialMaterial::OAuth {
                access_token: SecretBytes::new(b"access-secret".to_vec()),
                refresh_token: Some(SecretBytes::new(b"refresh-secret".to_vec())),
                expires_at_unix: Some(42),
                token_type: "Bearer".to_owned(),
                scopes: vec!["read".to_owned()],
                upstream_identity: Some("alice".to_owned()),
            },
            overrides: CredentialClientOverrides {
                client_id: Some("public-client".to_owned()),
                client_secret: Some(SecretBytes::new(b"client-secret".to_vec())),
                redirect_uri: None,
                scopes: None,
            },
        };
        let debug = format!("{submission:?}");
        assert!(!debug.contains("access-secret"));
        assert!(!debug.contains("refresh-secret"));
        assert!(!debug.contains("client-secret"));
    }
}
