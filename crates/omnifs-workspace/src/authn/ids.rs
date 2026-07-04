use crate::ids::{self as ids, ProviderName};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

pub use crate::ids::IdError;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SchemeId(String);

impl SchemeId {
    pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
        let value = value.into();
        ids::validate_key_part("scheme", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SchemeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for SchemeId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl FromStr for SchemeId {
    type Err = IdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AccountId(String);

impl AccountId {
    pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
        let value = value.into();
        ids::validate_account(&value)?;
        Ok(Self(value))
    }

    pub fn default_account() -> Self {
        Self("default".to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for AccountId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl FromStr for AccountId {
    type Err = IdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

/// Stable address for one host-managed HTTP credential.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CredentialId {
    /// The provider NAME slug (e.g. `github`), never the content `ProviderId`
    /// hash. Credentials key on the name so they survive provider upgrades.
    provider_name: ProviderName,
    scheme: SchemeId,
    account: AccountId,
}

impl CredentialId {
    pub fn new(
        provider_id: impl Into<String>,
        scheme: impl Into<String>,
        account: impl Into<String>,
    ) -> Result<Self, CredentialIdError> {
        let provider_id = provider_id.into();
        let scheme = scheme.into();
        let account = account.into();
        Ok(Self::from_parts(
            ProviderName::new(provider_id.clone())
                .map_err(|_| CredentialIdError::invalid("provider_id", &provider_id))?,
            SchemeId::new(scheme.clone())
                .map_err(|_| CredentialIdError::invalid("scheme", &scheme))?,
            AccountId::new(account.clone())
                .map_err(|_| CredentialIdError::invalid("account", &account))?,
        ))
    }

    pub fn from_parts(provider_name: ProviderName, scheme: SchemeId, account: AccountId) -> Self {
        Self {
            provider_name,
            scheme,
            account,
        }
    }

    /// The single credential-key derivation for a mount.
    ///
    /// `scheme` is the resolved scheme key: the spec's [`Auth::scheme`] value
    /// when present, otherwise the provider-manifest default resolved by the
    /// caller. The account comes from [`Auth::account`], defaulting to
    /// [`AccountId::default_account`]. This is exactly the derivation the CLI
    /// credential target and the host injector compute by hand today; both
    /// rewire onto this function in a later step.
    ///
    /// [`Auth::scheme`]: crate::mounts::Auth::scheme
    /// [`Auth::account`]: crate::mounts::Auth::account
    pub fn for_mount(
        provider: &ProviderName,
        auth: &crate::mounts::Auth,
        scheme: &str,
    ) -> Result<Self, CredentialIdError> {
        let scheme =
            SchemeId::new(scheme).map_err(|_| CredentialIdError::invalid("scheme", scheme))?;
        let account = match auth.account() {
            Some(account) => AccountId::new(account)
                .map_err(|_| CredentialIdError::invalid("account", account))?,
            None => AccountId::default_account(),
        };
        Ok(Self::from_parts(provider.clone(), scheme, account))
    }

    /// Stable account name used in the credential store.
    pub fn storage_key(&self) -> String {
        format!("{}:{}:{}", self.provider_name, self.scheme, self.account)
    }

    pub fn provider_name(&self) -> &str {
        self.provider_name.as_str()
    }

    pub fn scheme(&self) -> &str {
        self.scheme.as_str()
    }

    pub fn account(&self) -> &str {
        self.account.as_str()
    }
}

impl fmt::Display for CredentialId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.storage_key())
    }
}

impl FromStr for CredentialId {
    type Err = CredentialIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.splitn(3, ':');
        let provider_id = parts
            .next()
            .ok_or_else(|| CredentialIdError::malformed_storage_key(s))?;
        let scheme = parts
            .next()
            .ok_or_else(|| CredentialIdError::malformed_storage_key(s))?;
        let account = parts
            .next()
            .ok_or_else(|| CredentialIdError::malformed_storage_key(s))?;
        Self::new(provider_id, scheme, account)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
struct CredentialIdWire {
    provider_id: String,
    scheme: String,
    account: String,
}

impl Serialize for CredentialId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        CredentialIdWire {
            provider_id: self.provider_name.to_string(),
            scheme: self.scheme.to_string(),
            account: self.account.to_string(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CredentialId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = CredentialIdWire::deserialize(deserializer)?;
        Self::new(wire.provider_id, wire.scheme, wire.account).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CredentialIdError {
    #[error("invalid credential storage key: {value}")]
    MalformedStorageKey { value: String },
    #[error("credential {field} cannot be empty")]
    Empty { field: &'static str },
    #[error("credential {field} is too long: {len} bytes, max 128")]
    TooLong { field: &'static str, len: usize },
    #[error("invalid credential {field}: {value}")]
    InvalidPart { field: &'static str, value: String },
}

/// Discriminant for the two host-managed credential families.
///
/// Shared by `crate::creds` (`CredentialEntry.kind`), `crate::mounts`
/// (`Auth` / `AuthKind` accessors), and the CLI (`AuthSelection`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, strum::Display,
)]
#[serde(rename_all = "kebab-case")]
pub enum AuthKind {
    #[strum(serialize = "static-token")]
    StaticToken,
    #[serde(rename = "oauth")]
    #[strum(serialize = "oauth")]
    OAuth,
}

impl CredentialIdError {
    fn malformed_storage_key(value: &str) -> Self {
        Self::MalformedStorageKey {
            value: value.to_owned(),
        }
    }

    fn invalid(field: &'static str, value: &str) -> Self {
        if value.is_empty() {
            Self::Empty { field }
        } else if value.len() > 128 {
            Self::TooLong {
                field,
                len: value.len(),
            }
        } else {
            Self::InvalidPart {
                field,
                value: value.to_owned(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_id_keys_on_name_and_wire_is_byte_stable() {
        let id = CredentialId::new("github", "pat", "user").unwrap();
        assert_eq!(id.provider_name(), "github");
        assert_eq!(id.scheme(), "pat");
        assert_eq!(id.account(), "user");
        // The storage-key format is unchanged: name:scheme:account.
        assert_eq!(id.storage_key(), "github:pat:user");
        // The JSON wire keeps the field name `provider_id` so credentials.json
        // stays byte-stable across the slug -> ProviderName type rename.
        let value = serde_json::to_value(&id).unwrap();
        assert_eq!(value["provider_id"], "github");
        assert_eq!(value["scheme"], "pat");
        assert_eq!(value["account"], "user");
        assert_eq!(serde_json::from_value::<CredentialId>(value).unwrap(), id);
    }

    #[test]
    fn credential_id_rejects_invalid_name() {
        assert!(CredentialId::new("bad name!", "pat", "default").is_err());
    }

    /// Parity with the CLI derivation (`credential_target::internal_from_parts`):
    /// provider from the spec, scheme validated through `SchemeId`, account
    /// from `auth.account()` mapped through `AccountId::new` with
    /// `AccountId::default_account()` as the fallback.
    #[test]
    fn for_mount_matches_the_cli_derivation() {
        use crate::mounts::{Auth, StaticToken};

        let provider = ProviderName::new("github").unwrap();
        for (account, expected_account) in [(Some("work"), "work"), (None, "default")] {
            let auth = Auth::StaticToken(StaticToken {
                scheme: Some("pat".to_owned()),
                account: account.map(str::to_owned),
            });
            let cli_expected = CredentialId::from_parts(
                provider.clone(),
                SchemeId::new(auth.scheme().unwrap()).unwrap(),
                auth.account()
                    .map(AccountId::new)
                    .transpose()
                    .unwrap()
                    .unwrap_or_else(AccountId::default_account),
            );
            let got = CredentialId::for_mount(&provider, &auth, auth.scheme().unwrap()).unwrap();
            assert_eq!(got, cli_expected);
            assert_eq!(got.account(), expected_account);
            assert_eq!(got.storage_key(), format!("github:pat:{expected_account}"));
        }
    }

    /// Parity with the host injector derivation (`omnifs-engine/src/auth_inject.rs`),
    /// which now calls `for_mount` directly: account from the mount auth config
    /// or the literal "default", then `CredentialId::new(provider_name,
    /// scheme_key, account)` where the scheme key was resolved from the spec
    /// value or the manifest default.
    #[test]
    fn for_mount_matches_the_host_derivation() {
        use crate::mounts::{Auth, OAuth};

        let provider = ProviderName::new("linear").unwrap();
        for (spec_scheme, account) in [(None, None), (Some("oauth-app"), Some("personal"))] {
            let auth = Auth::OAuth(OAuth {
                scheme: spec_scheme.map(str::to_owned),
                account: account.map(str::to_owned),
                ..OAuth::default()
            });
            // The caller resolves the scheme: the spec value when present,
            // otherwise the manifest default (here: "oauth-app").
            let resolved_scheme = auth.scheme().unwrap_or("oauth-app");
            let host_account = auth
                .account()
                .map_or_else(|| "default".to_owned(), str::to_owned);
            let host_expected = CredentialId::new("linear", resolved_scheme, host_account).unwrap();
            let got = CredentialId::for_mount(&provider, &auth, resolved_scheme).unwrap();
            assert_eq!(got, host_expected);
            assert_eq!(got.storage_key(), host_expected.storage_key());
        }
    }

    #[test]
    fn for_mount_rejects_invalid_parts() {
        use crate::mounts::{Auth, StaticToken};

        let provider = ProviderName::new("github").unwrap();
        let auth = Auth::StaticToken(StaticToken {
            scheme: None,
            account: Some("bad/account".to_owned()),
        });
        assert!(CredentialId::for_mount(&provider, &auth, "pat").is_err());
        let auth = Auth::StaticToken(StaticToken::default());
        assert!(CredentialId::for_mount(&provider, &auth, "bad scheme!").is_err());
    }
}
