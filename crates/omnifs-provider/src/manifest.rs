//! Typed `omnifs.provider.json` manifest.

use crate::auth_wire::{
    AuthManifest, AuthScheme, DeviceCodeConfig, OAuthFlow, OauthScheme, PkceLoopbackConfig,
    PkceManualCodeConfig, StaticTokenScheme, TokenEndpointAuthMethod, TokenValidation,
};
use crate::config::ConfigSchema;
use crate::runtime_grants::{PreopenedPath, ProviderCapabilities};
use crate::sections::{ProviderMetadataError, is_hostname_only, validate_provider_manifest};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const PROVIDER_WIT_CONTRACT: &str = "omnifs:provider@0.4.0";

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderManifest {
    pub id: String,
    pub display_name: String,
    /// Filename of the provider WASM component.
    pub provider: String,
    pub default_mount: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<ContractEvidence>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<CapabilityEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<ProviderAuthManifest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<schemars::Schema>,
}

/// The contract a built provider component was compiled against: the
/// `omnifs:provider` WIT package version and the SDK version. The `#[provider]`
/// macro stamps this into the embedded manifest so the host can later detect a
/// provider built against an incompatible contract.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContractEvidence {
    pub wit: String,
    pub sdk: String,
}

impl ContractEvidence {
    #[must_use]
    pub fn current(sdk_version: impl Into<String>) -> Self {
        Self {
            wit: PROVIDER_WIT_CONTRACT.to_string(),
            sdk: sdk_version.into(),
        }
    }

    fn validate(&self) -> Result<(), ProviderMetadataError> {
        validate_non_empty("contract.wit", &self.wit)?;
        validate_non_empty("contract.sdk", &self.sdk)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum CapabilityEntry {
    Domain {
        value: String,
        why: String,
        #[serde(default)]
        dynamic: bool,
    },
    GitRepo {
        value: String,
        why: String,
        #[serde(default)]
        dynamic: bool,
    },
    UnixSocket {
        value: String,
        why: String,
        #[serde(default)]
        dynamic: bool,
    },
    PreopenedPath {
        value: PreopenedPath,
        why: String,
        #[serde(default)]
        dynamic: bool,
    },
    MemoryMb {
        value: u32,
        why: String,
        #[serde(default)]
        dynamic: bool,
    },
    FetchBlobBytes {
        value: u64,
        why: String,
        #[serde(default)]
        dynamic: bool,
    },
    ReadBlobBytes {
        value: u64,
        why: String,
        #[serde(default)]
        dynamic: bool,
    },
}

impl CapabilityEntry {
    #[must_use]
    pub fn why(&self) -> &str {
        match self {
            Self::Domain { why, .. }
            | Self::GitRepo { why, .. }
            | Self::UnixSocket { why, .. }
            | Self::PreopenedPath { why, .. }
            | Self::MemoryMb { why, .. }
            | Self::FetchBlobBytes { why, .. }
            | Self::ReadBlobBytes { why, .. } => why,
        }
    }

    #[must_use]
    pub fn is_dynamic(&self) -> bool {
        match self {
            Self::Domain { dynamic, .. }
            | Self::GitRepo { dynamic, .. }
            | Self::UnixSocket { dynamic, .. }
            | Self::PreopenedPath { dynamic, .. }
            | Self::MemoryMb { dynamic, .. }
            | Self::FetchBlobBytes { dynamic, .. }
            | Self::ReadBlobBytes { dynamic, .. } => *dynamic,
        }
    }
}

/// Provider auth block from `omnifs.provider.json`.
///
/// Deserialization applies the `inject` block to every scheme so that
/// `schemes` holds fully-resolved [`AuthScheme`] values ready for runtime use.
/// No separate transform step is needed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderAuthManifest {
    pub inject: AuthInject,
    /// Key of the scheme used by `omnifs init` when no explicit choice.
    pub default: String,
    pub schemes: BTreeMap<String, AuthScheme>,
}

impl ProviderAuthManifest {
    #[must_use]
    pub fn default_scheme(&self) -> Option<(&str, &AuthScheme)> {
        let scheme = self.schemes.get(&self.default)?;
        Some((self.default.as_str(), scheme))
    }

    /// Auth manifest derived from provider metadata for host HTTP injection.
    #[must_use]
    pub fn wasm_auth_manifest(&self) -> AuthManifest {
        let schemes = self.schemes.values().cloned().collect();
        AuthManifest { schemes }
    }

    fn validate(&self) -> Result<(), ProviderMetadataError> {
        validate_non_empty("auth.default", &self.default)?;
        validate_inject_domains(&self.inject.domains)?;
        if !self.schemes.contains_key(&self.default) {
            return Err(ProviderMetadataError::Validation(format!(
                "auth.default {:?} has no matching auth.schemes entry",
                self.default
            )));
        }
        for (key, scheme) in &self.schemes {
            validate_scheme(key, scheme, &self.inject)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Serialization for ProviderAuthManifest.
//
// We write back in the compact manifest format (inject + scheme keys) rather
// than the expanded wire format so that round-trip of provider JSON files is
// preserved.
// ---------------------------------------------------------------------------

/// On-disk compact form of an OAuth scheme inside `omnifs.provider.json`.
#[derive(Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "ManifestOauthScheme")]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawOauthScheme {
    display_name: String,
    client_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    scopes: Vec<String>,
    flow: RawOAuthFlow,
}

/// On-disk compact flow descriptor inside an OAuth scheme.
#[derive(Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "ManifestOAuthFlow")]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
#[serde(rename_all_fields = "camelCase")]
enum RawOAuthFlow {
    DeviceCode {
        authorization_endpoint: String,
        device_authorization_endpoint: String,
        token_endpoint: String,
    },
    PkceLoopback {
        authorization_endpoint: String,
        token_endpoint: String,
        redirect_uri_template: String,
    },
    PkceManualCode {
        authorization_endpoint: String,
        token_endpoint: String,
        redirect_uri: String,
    },
}

/// On-disk compact form of a static-token scheme inside `omnifs.provider.json`.
#[derive(Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "ManifestStaticTokenScheme")]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawStaticTokenScheme {
    description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    creation_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    validation: Option<TokenValidation>,
}

/// On-disk discriminant for a scheme entry.
#[derive(Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "ManifestAuthScheme")]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
enum RawAuthScheme {
    StaticToken(RawStaticTokenScheme),
    Oauth(RawOauthScheme),
}

/// Wire form of the whole auth block as it appears in `omnifs.provider.json`.
#[derive(Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "AuthBlock")]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawProviderAuthManifest {
    inject: AuthInject,
    /// Key of the scheme used by `omnifs init` when no explicit choice.
    default: String,
    schemes: BTreeMap<String, RawAuthScheme>,
}

impl JsonSchema for ProviderAuthManifest {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        RawProviderAuthManifest::schema_name()
    }

    fn json_schema(generator: &mut schemars::generate::SchemaGenerator) -> schemars::Schema {
        RawProviderAuthManifest::json_schema(generator)
    }
}

impl Serialize for ProviderAuthManifest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Serialize back to the compact manifest form by reversing the expansion.
        let raw_schemes: BTreeMap<String, RawAuthScheme> = self
            .schemes
            .iter()
            .map(|(key, scheme)| {
                let raw = match scheme {
                    AuthScheme::None => {
                        // None schemes cannot appear in provider manifests.
                        return Err(serde::ser::Error::custom(
                            "AuthScheme::None cannot be serialized as a provider manifest scheme",
                        ));
                    },
                    AuthScheme::StaticToken(s) => {
                        RawAuthScheme::StaticToken(RawStaticTokenScheme {
                            description: s.description.clone(),
                            creation_url: s.creation_url.clone(),
                            validation: s.validation.clone(),
                        })
                    },
                    AuthScheme::Oauth(o) => {
                        let flow = match &o.flow {
                            OAuthFlow::DeviceCode(d) => RawOAuthFlow::DeviceCode {
                                authorization_endpoint: o.authorization_endpoint.clone(),
                                device_authorization_endpoint: d
                                    .device_authorization_endpoint
                                    .clone(),
                                token_endpoint: o.token_endpoint.clone(),
                            },
                            OAuthFlow::PkceLoopback(p) => RawOAuthFlow::PkceLoopback {
                                authorization_endpoint: o.authorization_endpoint.clone(),
                                token_endpoint: o.token_endpoint.clone(),
                                redirect_uri_template: p.redirect_uri_template.clone(),
                            },
                            OAuthFlow::PkceManualCode(p) => RawOAuthFlow::PkceManualCode {
                                authorization_endpoint: o.authorization_endpoint.clone(),
                                token_endpoint: o.token_endpoint.clone(),
                                redirect_uri: p.redirect_uri.clone(),
                            },
                        };
                        RawAuthScheme::Oauth(RawOauthScheme {
                            display_name: o.display_name.clone(),
                            client_id: o.default_client_id.clone().unwrap_or_default(),
                            scopes: o.default_scopes.clone(),
                            flow,
                        })
                    },
                };
                Ok((key.clone(), raw))
            })
            .collect::<Result<_, S::Error>>()?;
        RawProviderAuthManifest {
            inject: self.inject.clone(),
            default: self.default.clone(),
            schemes: raw_schemes,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ProviderAuthManifest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawProviderAuthManifest::deserialize(deserializer)?;
        let schemes = raw
            .schemes
            .into_iter()
            .map(|(key, raw_scheme)| {
                let scheme = expand_raw_scheme(&key, raw_scheme, &raw.inject);
                (key, scheme)
            })
            .collect();
        Ok(Self {
            inject: raw.inject,
            default: raw.default,
            schemes,
        })
    }
}

fn expand_raw_scheme(key: &str, raw: RawAuthScheme, inject: &AuthInject) -> AuthScheme {
    match raw {
        RawAuthScheme::StaticToken(s) => AuthScheme::StaticToken(StaticTokenScheme {
            key: key.to_string(),
            header_name: Some(inject.header.clone()),
            value_prefix: inject.prefix.clone(),
            description: s.description,
            inject_domains: inject.domains.clone(),
            creation_url: s.creation_url,
            validation: s.validation,
        }),
        RawAuthScheme::Oauth(o) => {
            let refresh_token_rotates = matches!(o.flow, RawOAuthFlow::PkceLoopback { .. });
            let (authorization_endpoint, token_endpoint, flow) = match o.flow {
                RawOAuthFlow::DeviceCode {
                    authorization_endpoint,
                    device_authorization_endpoint,
                    token_endpoint,
                } => (
                    authorization_endpoint,
                    token_endpoint,
                    OAuthFlow::DeviceCode(DeviceCodeConfig {
                        device_authorization_endpoint,
                    }),
                ),
                RawOAuthFlow::PkceLoopback {
                    authorization_endpoint,
                    token_endpoint,
                    redirect_uri_template,
                } => (
                    authorization_endpoint,
                    token_endpoint,
                    OAuthFlow::PkceLoopback(PkceLoopbackConfig {
                        redirect_uri_template,
                    }),
                ),
                RawOAuthFlow::PkceManualCode {
                    authorization_endpoint,
                    token_endpoint,
                    redirect_uri,
                } => (
                    authorization_endpoint,
                    token_endpoint,
                    OAuthFlow::PkceManualCode(PkceManualCodeConfig { redirect_uri }),
                ),
            };
            AuthScheme::Oauth(OauthScheme {
                key: key.to_string(),
                display_name: o.display_name,
                authorization_endpoint,
                token_endpoint,
                revocation_endpoint: None,
                default_client_id: Some(o.client_id),
                default_scopes: o.scopes,
                flow,
                token_endpoint_auth: TokenEndpointAuthMethod::None,
                refresh_token_rotates,
                extra_authorize_params: Vec::new(),
                extra_token_params: Vec::new(),
                inject_domains: inject.domains.clone(),
                inject_header_name: Some(inject.header.clone()),
                inject_value_prefix: inject.prefix.clone(),
            })
        },
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthInject {
    pub domains: Vec<String>,
    pub header: String,
    pub prefix: String,
}

impl ProviderManifest {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ProviderMetadataError> {
        let value: serde_json::Value =
            serde_json::from_slice(bytes).map_err(ProviderMetadataError::Json)?;
        validate_provider_manifest(&value)?;
        let manifest: Self = serde_json::from_value(value).map_err(ProviderMetadataError::Json)?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn from_path(path: &std::path::Path) -> Result<Self, ProviderMetadataError> {
        let bytes = std::fs::read(path).map_err(|error| {
            ProviderMetadataError::Validation(format!("read {}: {error}", path.display()))
        })?;
        Self::from_bytes(&bytes)
    }

    #[must_use]
    pub fn default_scheme(&self) -> Option<(&str, &AuthScheme)> {
        self.auth.as_ref()?.default_scheme()
    }

    /// Auth manifest derived from provider metadata for host HTTP injection.
    #[must_use]
    pub fn wasm_auth_manifest(&self) -> Option<AuthManifest> {
        Some(self.auth.as_ref()?.wasm_auth_manifest())
    }

    fn validate(&self) -> Result<(), ProviderMetadataError> {
        validate_non_empty("id", &self.id)?;
        validate_non_empty("displayName", &self.display_name)?;
        validate_non_empty("provider", &self.provider)?;
        validate_non_empty("defaultMount", &self.default_mount)?;
        if let Some(contract) = &self.contract {
            contract.validate()?;
        }
        for entry in &self.capabilities {
            validate_non_empty("capabilities.why", entry.why())?;
        }
        if let Some(auth) = &self.auth {
            auth.validate()?;
        }
        if let Some(schema) = self.config_schema.as_ref() {
            jsonschema::meta::validate(schema.as_value()).map_err(|error| {
                ProviderMetadataError::Validation(format!("configSchema: {error}"))
            })?;
            ConfigSchema::parse(schema)?;
        }
        Ok(())
    }

    #[must_use]
    pub fn provider_capabilities(&self) -> ProviderCapabilities {
        let mut caps = ProviderCapabilities::default();
        for entry in &self.capabilities {
            match entry {
                CapabilityEntry::Domain { value, .. } => caps
                    .domains
                    .get_or_insert_with(Vec::new)
                    .push(value.clone()),
                CapabilityEntry::GitRepo { value, .. } => caps
                    .git_repos
                    .get_or_insert_with(Vec::new)
                    .push(value.clone()),
                CapabilityEntry::UnixSocket { value, .. } => caps
                    .unix_sockets
                    .get_or_insert_with(Vec::new)
                    .push(value.clone()),
                CapabilityEntry::PreopenedPath { value, .. } => caps
                    .preopened_paths
                    .get_or_insert_with(Vec::new)
                    .push(value.clone()),
                CapabilityEntry::MemoryMb { value, .. } => caps.max_memory_mb = Some(*value),
                CapabilityEntry::FetchBlobBytes { value, .. } => {
                    caps.max_fetch_blob_bytes = Some(*value);
                },
                CapabilityEntry::ReadBlobBytes { value, .. } => {
                    caps.max_read_blob_bytes = Some(*value);
                },
            }
        }
        caps
    }
}

fn validate_scheme(
    key: &str,
    scheme: &AuthScheme,
    inject: &AuthInject,
) -> Result<(), ProviderMetadataError> {
    match scheme {
        AuthScheme::None => {
            return Err(ProviderMetadataError::Validation(format!(
                "auth.schemes.{key}: None is not a valid provider scheme"
            )));
        },
        AuthScheme::StaticToken(static_token) => {
            validate_non_empty(
                &format!("auth.schemes.{key}.description"),
                &static_token.description,
            )?;
        },
        AuthScheme::Oauth(oauth) => {
            validate_non_empty(
                &format!("auth.schemes.{key}.displayName"),
                &oauth.display_name,
            )?;
            if let Some(client_id) = &oauth.default_client_id {
                validate_non_empty(&format!("auth.schemes.{key}.clientId"), client_id)?;
            }
            validate_oauth_flow(key, oauth)?;
        },
    }
    let _ = inject;
    Ok(())
}

fn validate_oauth_flow(key: &str, oauth: &OauthScheme) -> Result<(), ProviderMetadataError> {
    validate_https_endpoint(
        &format!("auth.schemes.{key}.flow.authorizationEndpoint"),
        &oauth.authorization_endpoint,
    )?;
    validate_https_endpoint(
        &format!("auth.schemes.{key}.flow.tokenEndpoint"),
        &oauth.token_endpoint,
    )?;
    match &oauth.flow {
        OAuthFlow::DeviceCode(d) => {
            validate_https_endpoint(
                &format!("auth.schemes.{key}.flow.deviceAuthorizationEndpoint"),
                &d.device_authorization_endpoint,
            )?;
        },
        OAuthFlow::PkceLoopback(p) => {
            if !p.redirect_uri_template.contains("{port}") {
                return Err(ProviderMetadataError::Validation(format!(
                    "auth.schemes.{key}.flow.redirectUriTemplate must contain {{port}}"
                )));
            }
        },
        OAuthFlow::PkceManualCode(p) => {
            if p.redirect_uri.contains("{port}") {
                return Err(ProviderMetadataError::Validation(format!(
                    "auth.schemes.{key}.flow.redirectUri must not contain {{port}}"
                )));
            }
        },
    }
    Ok(())
}

fn validate_non_empty(field: &str, value: &str) -> Result<(), ProviderMetadataError> {
    if value.trim().is_empty() {
        return Err(ProviderMetadataError::Validation(format!(
            "{field} must not be empty"
        )));
    }
    Ok(())
}

fn validate_https_endpoint(field: &str, endpoint: &str) -> Result<(), ProviderMetadataError> {
    if endpoint.starts_with("https://") {
        Ok(())
    } else {
        Err(ProviderMetadataError::Validation(format!(
            "{field} must start with https://"
        )))
    }
}

fn validate_inject_domains(domains: &[String]) -> Result<(), ProviderMetadataError> {
    for domain in domains {
        if !is_hostname_only(domain) {
            return Err(ProviderMetadataError::Validation(format!(
                "auth.inject.domains entry {domain:?} must be a hostname without scheme, path, port, or wildcard"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sections::{ProviderMetadataError, provider_manifest_json};
    use serde::Serialize;

    #[test]
    fn checked_in_oauth_provider_manifests_have_well_formed_auth() {
        for (provider_id, default_scheme) in [("github", "device"), ("linear", "oauth")] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../providers")
                .join(provider_id)
                .join("omnifs.provider.json");
            let bytes = std::fs::read(&path).unwrap_or_else(|error| {
                panic!("read {}: {error}", path.display());
            });
            let manifest = ProviderManifest::from_bytes(&bytes)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            let auth = manifest.auth.as_ref().unwrap_or_else(|| {
                panic!("{provider_id} manifest must declare auth");
            });
            assert_eq!(auth.default, default_scheme);
            assert!(
                auth.schemes.contains_key(default_scheme),
                "{provider_id} auth must declare default scheme `{default_scheme}`"
            );
            let wasm_auth = auth.wasm_auth_manifest();
            assert!(
                !wasm_auth.schemes.is_empty(),
                "{provider_id} wasm auth manifest must expose schemes"
            );
        }
    }

    #[test]
    fn capability_entry_dynamic_round_trips() {
        let omitted: CapabilityEntry = serde_json::from_str(
            r#"{"kind":"domain","value":"api.example.com","why":"fetch data"}"#,
        )
        .unwrap();
        assert!(!omitted.is_dynamic());

        let explicit: CapabilityEntry = serde_json::from_str(
            r#"{"kind":"unixSocket","value":"configured socket","dynamic":true,"why":"connect"}"#,
        )
        .unwrap();
        assert!(explicit.is_dynamic());
        let encoded = serde_json::to_value(&explicit).unwrap();
        assert_eq!(encoded["dynamic"], true);
    }

    #[test]
    fn checked_in_provider_manifest_matches_generated() {
        let schema_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("schema/omnifs.provider.schema.json");
        let checked_in: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(schema_path).unwrap()).unwrap();
        let generated = serde_json::to_value(provider_manifest_json()).unwrap();

        assert_eq!(checked_in, generated);
    }

    #[test]
    fn checked_in_provider_manifests_parse_as_typed_metadata() {
        let providers_dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../providers");
        let mut parsed = Vec::new();
        for entry in std::fs::read_dir(providers_dir).unwrap() {
            let path = entry.unwrap().path().join("omnifs.provider.json");
            if !path.exists() {
                continue;
            }
            let bytes = std::fs::read(&path).unwrap();
            let manifest = ProviderManifest::from_bytes(&bytes)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            if manifest.id != "test-provider" {
                parsed.push(manifest.id);
            }
        }
        parsed.sort();

        assert_eq!(
            parsed,
            [
                "arxiv",
                "db",
                "dns",
                "docker",
                "github",
                "kubernetes",
                "linear"
            ]
        );
    }

    #[test]
    fn provider_manifest_contract_evidence_round_trips() {
        let manifest = ProviderManifest::from_bytes(
            br#"{
                "id": "demo",
                "displayName": "Demo",
                "provider": "demo.wasm",
                "defaultMount": "demo",
                "contract": {
                    "wit": "omnifs:provider@0.4.0",
                    "sdk": "0.2.1"
                }
            }"#,
        )
        .unwrap();

        assert_eq!(
            manifest.contract,
            Some(ContractEvidence {
                wit: "omnifs:provider@0.4.0".to_string(),
                sdk: "0.2.1".to_string(),
            })
        );
    }

    #[test]
    fn provider_wit_contract_constant_matches_wit_package() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../omnifs-wit/wit/provider.wit");
        let wit = std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!("read {}: {error}", path.display());
        });
        let package = wit
            .lines()
            .find(|line| line.starts_with("package "))
            .expect("provider.wit declares a package");

        assert_eq!(package, format!("package {PROVIDER_WIT_CONTRACT};"));
    }

    #[test]
    fn read_provider_manifest_rejects_fractional_memory_capability() {
        let err = ProviderManifest::from_bytes(
            br#"{
                "id": "bad",
                "displayName": "Bad",
                "provider": "bad.wasm",
                "defaultMount": "bad",
                "capabilities": [
                    { "kind": "memoryMb", "value": 1.5, "why": "bad" }
                ]
            }"#,
        )
        .unwrap_err();

        assert!(matches!(
            err,
            ProviderMetadataError::Schema(_) | ProviderMetadataError::Json(_)
        ));
    }

    #[test]
    fn read_provider_manifest_rejects_out_of_range_memory_capability() {
        let err = ProviderManifest::from_bytes(
            br#"{
                "id": "bad",
                "displayName": "Bad",
                "provider": "bad.wasm",
                "defaultMount": "bad",
                "capabilities": [
                    { "kind": "memoryMb", "value": 4294967296, "why": "bad" }
                ]
            }"#,
        )
        .unwrap_err();

        assert!(matches!(
            err,
            ProviderMetadataError::Schema(_) | ProviderMetadataError::Json(_)
        ));
    }

    #[test]
    fn read_provider_metadata_rejects_invalid_config_schema() {
        let bytes = br#"{
            "id": "bad",
            "displayName": "Bad",
            "provider": "bad.wasm",
            "defaultMount": "bad",
            "configSchema": {
                "type": 7
            }
        }"#;

        let error = ProviderManifest::from_bytes(bytes).unwrap_err();

        assert!(
            matches!(error, ProviderMetadataError::Validation(message) if message.contains("configSchema"))
        );
    }

    #[test]
    fn provider_metadata_rejects_http_oauth_endpoint() {
        let mut manifest = oauth_provider_manifest();
        let auth = manifest.auth.as_mut().expect("oauth auth");
        let AuthScheme::Oauth(oauth) = auth.schemes.get_mut("oauth").expect("oauth scheme") else {
            panic!("expected oauth scheme");
        };
        oauth.authorization_endpoint = "http://linear.app/oauth/authorize".to_string();

        let error = encode_provider_manifest(&manifest).unwrap_err();
        assert!(
            matches!(error, ProviderMetadataError::Validation(message) if message.contains("authorizationEndpoint"))
        );
    }

    #[test]
    fn provider_metadata_rejects_loopback_template_without_port() {
        let mut manifest = oauth_provider_manifest();
        let auth = manifest.auth.as_mut().expect("oauth auth");
        let AuthScheme::Oauth(oauth) = auth.schemes.get_mut("oauth").expect("oauth scheme") else {
            panic!("expected oauth scheme");
        };
        oauth.flow = OAuthFlow::PkceLoopback(PkceLoopbackConfig {
            redirect_uri_template: "http://127.0.0.1/callback".to_string(),
        });
        // Authorization and token endpoints stay the same.

        let error = encode_provider_manifest(&manifest).unwrap_err();
        assert!(
            matches!(error, ProviderMetadataError::Validation(message) if message.contains("{port}"))
        );
    }

    #[test]
    fn provider_metadata_rejects_manual_code_redirect_uri_with_port_template() {
        let mut manifest = oauth_provider_manifest();
        let auth = manifest.auth.as_mut().expect("oauth auth");
        let AuthScheme::Oauth(oauth) = auth.schemes.get_mut("oauth").expect("oauth scheme") else {
            panic!("expected oauth scheme");
        };
        oauth.flow = OAuthFlow::PkceManualCode(PkceManualCodeConfig {
            redirect_uri: "https://example.com/callback/{port}".to_string(),
        });

        let error = encode_provider_manifest(&manifest).unwrap_err();
        assert!(
            matches!(error, ProviderMetadataError::Validation(message) if message.contains("must not contain {port}"))
        );
    }

    #[test]
    fn provider_metadata_rejects_url_shaped_inject_domain() {
        let mut manifest = oauth_provider_manifest();
        manifest.auth.as_mut().expect("oauth auth").inject.domains =
            vec!["https://api.linear.app".to_string()];

        let error = encode_provider_manifest(&manifest).unwrap_err();
        assert!(
            matches!(error, ProviderMetadataError::Validation(message) if message.contains("auth.inject.domains"))
        );
    }

    #[test]
    fn provider_metadata_rejects_wildcard_inject_domain() {
        let mut manifest = oauth_provider_manifest();
        manifest.auth.as_mut().expect("oauth auth").inject.domains =
            vec!["*.linear.app".to_string()];

        let error = encode_provider_manifest(&manifest).unwrap_err();
        assert!(
            matches!(error, ProviderMetadataError::Validation(message) if message.contains("auth.inject.domains"))
        );
    }

    fn oauth_provider_manifest() -> ProviderManifest {
        // Build via the compact JSON form so the custom Deserialize runs.
        let json = serde_json::json!({
            "id": "linear",
            "displayName": "Linear",
            "provider": "omnifs_provider_linear.wasm",
            "defaultMount": "linear",
            "auth": {
                "inject": {
                    "domains": ["api.linear.app"],
                    "header": "Authorization",
                    "prefix": "Bearer "
                },
                "default": "oauth",
                "schemes": {
                    "oauth": {
                        "type": "oauth",
                        "displayName": "Linear OAuth",
                        "clientId": "client-id",
                        "scopes": ["read"],
                        "flow": {
                            "kind": "pkceLoopback",
                            "authorizationEndpoint": "https://linear.app/oauth/authorize",
                            "tokenEndpoint": "https://api.linear.app/oauth/token",
                            "redirectUriTemplate": "http://127.0.0.1:{port}/callback"
                        }
                    }
                }
            }
        });
        serde_json::from_value(json).expect("oauth_provider_manifest parse")
    }

    fn encode_provider_manifest(
        manifest: &ProviderManifest,
    ) -> Result<ProviderManifest, ProviderMetadataError> {
        let bytes = json(manifest);
        ProviderManifest::from_bytes(&bytes)
    }

    fn json<T: Serialize>(value: &T) -> Vec<u8> {
        serde_json::to_vec(value).unwrap()
    }
}
