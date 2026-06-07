//! Typed `omnifs.provider.json` manifest.

use crate::auth_wire::{
    AuthManifest, AuthScheme, DeviceCodeConfig, OAuthFlow, OauthScheme, PkceLoopbackConfig,
    PkceManualCodeConfig, StaticTokenScheme, TokenEndpointAuthMethod,
};
use crate::config::ConfigSchema;
use crate::runtime_grants::{PreopenedPath, ProviderCapabilities};
use crate::sections::{ProviderMetadataError, is_hostname_only, validate_provider_manifest};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderManifest {
    pub id: String,
    pub display_name: String,
    /// Filename of the provider WASM component.
    pub provider: String,
    pub default_mount: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<CapabilityEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<ProviderAuthManifest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<schemars::Schema>,
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

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(rename = "AuthBlock")]
pub struct ProviderAuthManifest {
    pub inject: AuthInject,
    /// Key of the scheme used by `omnifs init` when no explicit choice.
    pub default: String,
    pub schemes: BTreeMap<String, ManifestAuthScheme>,
}

impl ProviderAuthManifest {
    #[must_use]
    pub fn default_scheme(&self) -> Option<(&str, &ManifestAuthScheme)> {
        let scheme = self.schemes.get(&self.default)?;
        Some((self.default.as_str(), scheme))
    }

    /// Auth manifest derived from provider metadata for host HTTP injection.
    #[must_use]
    pub fn wasm_auth_manifest(&self) -> AuthManifest {
        let schemes = self
            .schemes
            .iter()
            .map(|(key, scheme)| scheme.to_wasm_scheme(key, &self.inject))
            .collect();
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
            scheme.validate(key, &self.inject)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthInject {
    pub domains: Vec<String>,
    pub header: String,
    pub prefix: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum ManifestAuthScheme {
    StaticToken(ManifestStaticTokenScheme),
    Oauth(ManifestOauthScheme),
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TokenValidation {
    pub method: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub expect_status: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json_pointer: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extract: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ManifestStaticTokenScheme {
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creation_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation: Option<TokenValidation>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ManifestOauthScheme {
    pub display_name: String,
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    pub flow: ManifestOAuthFlow,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
#[schemars(rename_all = "camelCase")]
pub enum ManifestOAuthFlow {
    #[serde(rename_all = "camelCase")]
    DeviceCode {
        authorization_endpoint: String,
        device_authorization_endpoint: String,
        token_endpoint: String,
    },
    #[serde(rename_all = "camelCase")]
    PkceLoopback {
        authorization_endpoint: String,
        token_endpoint: String,
        redirect_uri_template: String,
    },
    #[serde(rename_all = "camelCase")]
    PkceManualCode {
        authorization_endpoint: String,
        token_endpoint: String,
        redirect_uri: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WasmOAuthFlowParts {
    authorization_endpoint: String,
    token_endpoint: String,
    flow: OAuthFlow,
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
    pub fn default_scheme(&self) -> Option<(&str, &ManifestAuthScheme)> {
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

impl ManifestAuthScheme {
    fn validate(&self, key: &str, inject: &AuthInject) -> Result<(), ProviderMetadataError> {
        match self {
            ManifestAuthScheme::StaticToken(static_token) => {
                validate_non_empty(
                    &format!("auth.schemes.{key}.description"),
                    &static_token.description,
                )?;
            },
            ManifestAuthScheme::Oauth(oauth) => {
                validate_non_empty(
                    &format!("auth.schemes.{key}.displayName"),
                    &oauth.display_name,
                )?;
                validate_non_empty(&format!("auth.schemes.{key}.clientId"), &oauth.client_id)?;
                oauth.flow.validate(key)?;
            },
        }
        let _ = inject;
        Ok(())
    }

    fn to_wasm_scheme(&self, key: &str, inject: &AuthInject) -> AuthScheme {
        match self {
            ManifestAuthScheme::StaticToken(static_token) => {
                AuthScheme::StaticToken(StaticTokenScheme {
                    key: key.to_string(),
                    header_name: Some(inject.header.clone()),
                    value_prefix: inject.prefix.clone(),
                    description: static_token.description.clone(),
                    inject_domains: inject.domains.clone(),
                })
            },
            ManifestAuthScheme::Oauth(oauth) => {
                let flow_parts = oauth.flow.to_wasm_parts();
                AuthScheme::Oauth(OauthScheme {
                    key: key.to_string(),
                    display_name: oauth.display_name.clone(),
                    authorization_endpoint: flow_parts.authorization_endpoint,
                    token_endpoint: flow_parts.token_endpoint,
                    revocation_endpoint: None,
                    default_client_id: Some(oauth.client_id.clone()),
                    default_scopes: oauth.scopes.clone(),
                    flow: flow_parts.flow,
                    token_endpoint_auth: TokenEndpointAuthMethod::None,
                    refresh_token_rotates: matches!(
                        oauth.flow,
                        ManifestOAuthFlow::PkceLoopback { .. }
                    ),
                    extra_authorize_params: Vec::new(),
                    extra_token_params: Vec::new(),
                    inject_domains: inject.domains.clone(),
                    inject_header_name: Some(inject.header.clone()),
                    inject_value_prefix: inject.prefix.clone(),
                })
            },
        }
    }
}

impl ManifestOAuthFlow {
    fn validate(&self, key: &str) -> Result<(), ProviderMetadataError> {
        match self {
            ManifestOAuthFlow::DeviceCode {
                authorization_endpoint,
                device_authorization_endpoint,
                token_endpoint,
            } => {
                validate_https_endpoint(
                    &format!("auth.schemes.{key}.flow.authorizationEndpoint"),
                    authorization_endpoint,
                )?;
                validate_https_endpoint(
                    &format!("auth.schemes.{key}.flow.deviceAuthorizationEndpoint"),
                    device_authorization_endpoint,
                )?;
                validate_https_endpoint(
                    &format!("auth.schemes.{key}.flow.tokenEndpoint"),
                    token_endpoint,
                )?;
            },
            ManifestOAuthFlow::PkceLoopback {
                authorization_endpoint,
                token_endpoint,
                redirect_uri_template,
            } => {
                validate_https_endpoint(
                    &format!("auth.schemes.{key}.flow.authorizationEndpoint"),
                    authorization_endpoint,
                )?;
                validate_https_endpoint(
                    &format!("auth.schemes.{key}.flow.tokenEndpoint"),
                    token_endpoint,
                )?;
                if !redirect_uri_template.contains("{port}") {
                    return Err(ProviderMetadataError::Validation(format!(
                        "auth.schemes.{key}.flow.redirectUriTemplate must contain {{port}}"
                    )));
                }
            },
            ManifestOAuthFlow::PkceManualCode {
                authorization_endpoint,
                token_endpoint,
                redirect_uri,
            } => {
                validate_https_endpoint(
                    &format!("auth.schemes.{key}.flow.authorizationEndpoint"),
                    authorization_endpoint,
                )?;
                validate_https_endpoint(
                    &format!("auth.schemes.{key}.flow.tokenEndpoint"),
                    token_endpoint,
                )?;
                if redirect_uri.contains("{port}") {
                    return Err(ProviderMetadataError::Validation(format!(
                        "auth.schemes.{key}.flow.redirectUri must not contain {{port}}"
                    )));
                }
            },
        }
        Ok(())
    }

    fn to_wasm_parts(&self) -> WasmOAuthFlowParts {
        match self {
            ManifestOAuthFlow::DeviceCode {
                authorization_endpoint,
                device_authorization_endpoint,
                token_endpoint,
            } => WasmOAuthFlowParts {
                authorization_endpoint: authorization_endpoint.clone(),
                token_endpoint: token_endpoint.clone(),
                flow: OAuthFlow::DeviceCode(DeviceCodeConfig {
                    device_authorization_endpoint: device_authorization_endpoint.clone(),
                }),
            },
            ManifestOAuthFlow::PkceLoopback {
                authorization_endpoint,
                token_endpoint,
                redirect_uri_template,
            } => WasmOAuthFlowParts {
                authorization_endpoint: authorization_endpoint.clone(),
                token_endpoint: token_endpoint.clone(),
                flow: OAuthFlow::PkceLoopback(PkceLoopbackConfig {
                    redirect_uri_template: redirect_uri_template.clone(),
                }),
            },
            ManifestOAuthFlow::PkceManualCode {
                authorization_endpoint,
                token_endpoint,
                redirect_uri,
            } => WasmOAuthFlowParts {
                authorization_endpoint: authorization_endpoint.clone(),
                token_endpoint: token_endpoint.clone(),
                flow: OAuthFlow::PkceManualCode(PkceManualCodeConfig {
                    redirect_uri: redirect_uri.clone(),
                }),
            },
        }
    }
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
    use std::collections::BTreeMap;

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

        assert_eq!(parsed, ["arxiv", "db", "dns", "docker", "github", "linear"]);
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
        let ManifestAuthScheme::Oauth(oauth) = auth.schemes.get_mut("oauth").expect("oauth scheme")
        else {
            panic!("expected oauth scheme");
        };
        let ManifestOAuthFlow::PkceLoopback {
            authorization_endpoint,
            ..
        } = &mut oauth.flow
        else {
            panic!("expected pkce loopback flow");
        };
        *authorization_endpoint = "http://linear.app/oauth/authorize".to_string();

        let error = encode_provider_manifest(&manifest).unwrap_err();
        assert!(
            matches!(error, ProviderMetadataError::Validation(message) if message.contains("authorizationEndpoint"))
        );
    }

    #[test]
    fn provider_metadata_rejects_loopback_template_without_port() {
        let mut manifest = oauth_provider_manifest();
        let auth = manifest.auth.as_mut().expect("oauth auth");
        let ManifestAuthScheme::Oauth(oauth) = auth.schemes.get_mut("oauth").expect("oauth scheme")
        else {
            panic!("expected oauth scheme");
        };
        oauth.flow = ManifestOAuthFlow::PkceLoopback {
            authorization_endpoint: "https://linear.app/oauth/authorize".to_string(),
            token_endpoint: "https://api.linear.app/oauth/token".to_string(),
            redirect_uri_template: "http://127.0.0.1/callback".to_string(),
        };

        let error = encode_provider_manifest(&manifest).unwrap_err();
        assert!(
            matches!(error, ProviderMetadataError::Validation(message) if message.contains("{port}"))
        );
    }

    #[test]
    fn provider_metadata_rejects_manual_code_redirect_uri_with_port_template() {
        let mut manifest = oauth_provider_manifest();
        let auth = manifest.auth.as_mut().expect("oauth auth");
        let ManifestAuthScheme::Oauth(oauth) = auth.schemes.get_mut("oauth").expect("oauth scheme")
        else {
            panic!("expected oauth scheme");
        };
        oauth.flow = ManifestOAuthFlow::PkceManualCode {
            authorization_endpoint: "https://linear.app/oauth/authorize".to_string(),
            token_endpoint: "https://api.linear.app/oauth/token".to_string(),
            redirect_uri: "https://example.com/callback/{port}".to_string(),
        };

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
        ProviderManifest {
            id: "linear".to_string(),
            display_name: "Linear".to_string(),
            provider: "omnifs_provider_linear.wasm".to_string(),
            default_mount: "linear".to_string(),
            capabilities: Vec::new(),
            auth: Some(ProviderAuthManifest {
                inject: AuthInject {
                    domains: vec!["api.linear.app".to_string()],
                    header: "Authorization".to_string(),
                    prefix: "Bearer ".to_string(),
                },
                default: "oauth".to_string(),
                schemes: BTreeMap::from([(
                    "oauth".to_string(),
                    ManifestAuthScheme::Oauth(ManifestOauthScheme {
                        display_name: "Linear OAuth".to_string(),
                        client_id: "client-id".to_string(),
                        scopes: vec!["read".to_string()],
                        flow: ManifestOAuthFlow::PkceLoopback {
                            authorization_endpoint: "https://linear.app/oauth/authorize"
                                .to_string(),
                            token_endpoint: "https://api.linear.app/oauth/token".to_string(),
                            redirect_uri_template: "http://127.0.0.1:{port}/callback".to_string(),
                        },
                    }),
                )]),
            }),
            config_schema: None,
        }
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
