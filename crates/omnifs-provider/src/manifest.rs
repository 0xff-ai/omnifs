//! Typed `omnifs.provider.json` manifest.

use crate::auth_wire::{
    AuthManifest, AuthScheme, ClientSideTokenConfig, DeviceCodeConfig, OAuthFlow, OauthScheme,
    PkceLoopbackConfig, PkceManualCodeConfig, SchemeGuidance, StaticTokenScheme,
    TokenEndpointAuthMethod, TokenValidation,
};
use crate::config::ConfigSchema;
use crate::sections::{ProviderMetadataError, is_hostname_only, validate_provider_manifest};
use omnifs_caps::{Grants, Need};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const DEFAULT_CLIENT_SIDE_TOKEN_REDIRECT_URI_TEMPLATE: &str = "http://127.0.0.1:{port}/callback";
pub const PROVIDER_WIT_CONTRACT: &str = "omnifs:provider@0.4.0";

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderManifest {
    pub id: String,
    pub display_name: String,
    /// Filename of the provider WASM component.
    pub provider: String,
    pub default_mount: String,
    /// Provider crate version (`CARGO_PKG_VERSION`), stamped by the `#[provider]`
    /// macro. Informational catalog/UI context, never identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_evidence: Option<BuildEvidence>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<Need>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<ProviderAuthManifest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<schemars::Schema>,
}

/// Build provenance for a compiled provider component: the `omnifs:provider`
/// WIT package version and the SDK version it was built against. The
/// `#[provider]` macro stamps this into the embedded manifest so the host can
/// later detect a provider built against an incompatible contract.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuildEvidence {
    pub wit: String,
    pub sdk: String,
}

impl BuildEvidence {
    #[must_use]
    pub fn current(sdk_version: impl Into<String>) -> Self {
        Self {
            wit: PROVIDER_WIT_CONTRACT.to_string(),
            sdk: sdk_version.into(),
        }
    }

    fn validate(&self) -> Result<(), ProviderMetadataError> {
        validate_non_empty("buildEvidence.wit", &self.wit)?;
        validate_non_empty("buildEvidence.sdk", &self.sdk)?;
        Ok(())
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
    /// Per-scheme display guidance, keyed by the same scheme key as `schemes`.
    /// Display metadata only; never affects header injection.
    pub guidance: BTreeMap<String, SchemeGuidance>,
}

impl ProviderAuthManifest {
    #[must_use]
    pub fn default_scheme(&self) -> Option<(&str, &AuthScheme)> {
        let scheme = self.schemes.get(&self.default)?;
        Some((self.default.as_str(), scheme))
    }

    /// Provider-supplied setup guidance for a scheme, or the empty default when
    /// the provider declared none.
    #[must_use]
    pub fn guidance_for(&self, scheme_key: &str) -> SchemeGuidance {
        self.guidance.get(scheme_key).cloned().unwrap_or_default()
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
            // A bring-your-own-app OAuth scheme (no shipped client id) forces the
            // user to create their own app; it must say how.
            if let AuthScheme::Oauth(oauth) = scheme
                && oauth.default_client_id.is_none()
            {
                let guidance = self.guidance_for(key);
                if guidance.setup_steps.is_empty() && guidance.docs_url.is_none() {
                    return Err(ProviderMetadataError::Validation(format!(
                        "auth.schemes.{key}: OAuth scheme ships no clientId, so it must declare `setup` steps or a `docsUrl` explaining how to create an app"
                    )));
                }
            }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    scopes: Vec<String>,
    flow: RawOAuthFlow,
    /// How the host authenticates at the token endpoint. `none` (the default)
    /// suits public PKCE clients; confidential clients that must present a
    /// secret use `clientSecretPost` or `clientSecretBasic`.
    #[serde(default, skip_serializing_if = "TokenEndpointAuthMethod::is_none")]
    token_endpoint_auth: TokenEndpointAuthMethod,
    /// One-line summary shown in scheme pickers and `omnifs auth explain`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    /// Ordered provider-specific prerequisite steps (create an OAuth app,
    /// enable an API). Required when no `clientId` is shipped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    setup: Vec<String>,
    /// Link to provider documentation for this auth path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    docs_url: Option<String>,
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
    ClientSideToken {
        authorization_endpoint: String,
        token_endpoint: String,
        #[serde(
            default,
            alias = "redirectUri",
            skip_serializing_if = "Option::is_none"
        )]
        redirect_uri_template: Option<String>,
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
    /// One-line summary shown in scheme pickers and `omnifs auth explain`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    /// Ordered provider-specific prerequisite steps beyond creating the token.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    setup: Vec<String>,
    /// Link to provider documentation for this auth path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    docs_url: Option<String>,
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
                let guidance = self.guidance.get(key).cloned().unwrap_or_default();
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
                            summary: guidance.summary,
                            setup: guidance.setup_steps,
                            docs_url: guidance.docs_url,
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
                            OAuthFlow::ClientSideToken(p) => RawOAuthFlow::ClientSideToken {
                                authorization_endpoint: o.authorization_endpoint.clone(),
                                token_endpoint: o.token_endpoint.clone(),
                                redirect_uri_template: (p.redirect_uri_template
                                    != DEFAULT_CLIENT_SIDE_TOKEN_REDIRECT_URI_TEMPLATE)
                                    .then(|| p.redirect_uri_template.clone()),
                            },
                        };
                        RawAuthScheme::Oauth(RawOauthScheme {
                            display_name: o.display_name.clone(),
                            client_id: o.default_client_id.clone(),
                            scopes: o.default_scopes.clone(),
                            flow,
                            token_endpoint_auth: o.token_endpoint_auth,
                            summary: guidance.summary,
                            setup: guidance.setup_steps,
                            docs_url: guidance.docs_url,
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
        let mut schemes = BTreeMap::new();
        let mut guidance = BTreeMap::new();
        for (key, raw_scheme) in raw.schemes {
            let (scheme, scheme_guidance) = expand_raw_scheme(&key, raw_scheme, &raw.inject);
            if !scheme_guidance.is_empty() {
                guidance.insert(key.clone(), scheme_guidance);
            }
            schemes.insert(key, scheme);
        }
        Ok(Self {
            inject: raw.inject,
            default: raw.default,
            schemes,
            guidance,
        })
    }
}

fn expand_raw_scheme(
    key: &str,
    raw: RawAuthScheme,
    inject: &AuthInject,
) -> (AuthScheme, SchemeGuidance) {
    match raw {
        RawAuthScheme::StaticToken(s) => {
            let guidance = SchemeGuidance {
                summary: s.summary,
                setup_steps: s.setup,
                docs_url: s.docs_url,
            };
            let scheme = AuthScheme::StaticToken(StaticTokenScheme {
                key: key.to_string(),
                header_name: Some(inject.header.clone()),
                value_prefix: inject.prefix.clone(),
                description: s.description,
                inject_domains: inject.domains.clone(),
                creation_url: s.creation_url,
                validation: s.validation,
            });
            (scheme, guidance)
        },
        RawAuthScheme::Oauth(o) => {
            let guidance = SchemeGuidance {
                summary: o.summary,
                setup_steps: o.setup,
                docs_url: o.docs_url,
            };
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
                RawOAuthFlow::ClientSideToken {
                    authorization_endpoint,
                    token_endpoint,
                    redirect_uri_template,
                } => (
                    authorization_endpoint,
                    token_endpoint,
                    OAuthFlow::ClientSideToken(ClientSideTokenConfig {
                        redirect_uri_template: redirect_uri_template.unwrap_or_else(|| {
                            DEFAULT_CLIENT_SIDE_TOKEN_REDIRECT_URI_TEMPLATE.to_string()
                        }),
                    }),
                ),
            };
            let scheme = AuthScheme::Oauth(OauthScheme {
                key: key.to_string(),
                display_name: o.display_name,
                authorization_endpoint,
                token_endpoint,
                revocation_endpoint: None,
                default_client_id: o.client_id,
                default_scopes: o.scopes,
                flow,
                token_endpoint_auth: o.token_endpoint_auth,
                refresh_token_rotates,
                extra_authorize_params: Vec::new(),
                extra_token_params: Vec::new(),
                inject_domains: inject.domains.clone(),
                inject_header_name: Some(inject.header.clone()),
                inject_value_prefix: inject.prefix.clone(),
            });
            (scheme, guidance)
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
        // `id` is the provider name slug; reject anything the name newtype would
        // reject so `ProviderMeta` conversion never fails past this parse boundary.
        omnifs_core::provider::ProviderName::new(self.id.as_str())
            .map_err(|error| ProviderMetadataError::Validation(error.to_string()))?;
        validate_non_empty("displayName", &self.display_name)?;
        validate_non_empty("provider", &self.provider)?;
        validate_non_empty("defaultMount", &self.default_mount)?;
        if let Some(build_evidence) = &self.build_evidence {
            build_evidence.validate()?;
        }
        for entry in &self.capabilities {
            validate_non_empty("capabilities.why", entry.why())?;
            // A dynamic grant is resolved at mount-start from a config field the
            // provider marks with `x-omnifs-resource`: a unix socket into the
            // callout allowlist, a preopened path into a WASI preopen. Any other
            // dynamic kind has no resolver and would resolve to an empty
            // allowlist, denying the provider at its first callout. Reject those
            // at the manifest boundary.
            if entry.is_dynamic()
                && !matches!(entry, Need::UnixSocket { .. } | Need::PreopenedPath { .. })
            {
                return Err(ProviderMetadataError::Validation(
                    "only unixSocket and preopenedPath capabilities may declare \
                     `dynamic: true`; a dynamic capability of another kind cannot be \
                     resolved at runtime"
                        .to_string(),
                ));
            }
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

    /// The capabilities this provider declares it needs, lowered into a grant
    /// set. Used by `omnifs init` to seed a mount's explicit grants; never used
    /// to grant at runtime (the spec is the grant authority).
    #[must_use]
    pub fn provider_capabilities(&self) -> Grants {
        Grants::from_needs(&self.capabilities)
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
        OAuthFlow::ClientSideToken(p) => {
            if !p.redirect_uri_template.contains("{port}")
                && !is_fixed_loopback_redirect(&p.redirect_uri_template)
            {
                return Err(ProviderMetadataError::Validation(format!(
                    "auth.schemes.{key}.flow.redirectUriTemplate must contain {{port}} or use http://localhost:<port>"
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

fn is_fixed_loopback_redirect(value: &str) -> bool {
    let Some(rest) = value
        .strip_prefix("http://localhost:")
        .or_else(|| value.strip_prefix("http://127.0.0.1:"))
    else {
        return false;
    };
    let Some(port) = rest.split('/').next() else {
        return false;
    };
    port.parse::<u16>().is_ok()
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
    use crate::sections::ProviderMetadataError;
    use serde::Serialize;

    macro_rules! cases {
        ($( ($label:expr, $input:expr, $pred:expr) ),+ $(,)?) => {{
            $( {
                let error = ProviderManifest::from_bytes($input).unwrap_err();
                assert!(
                    $pred(&error),
                    "{}: unexpected error: {error}",
                    $label
                );
            } )+
        }};
    }

    macro_rules! reject_oauth_surface {
        ($( ($needle:expr, |$manifest:ident| $mutate:expr) ),+ $(,)?) => {{
            $( {
                let mut $manifest = oauth_provider_manifest();
                $mutate;
                let error = encode_provider_manifest(&$manifest).unwrap_err();
                assert!(
                    matches!(
                        error,
                        ProviderMetadataError::Validation(ref message) if message.contains($needle)
                    ),
                    "needle {needle:?}: unexpected error: {error}",
                    needle = $needle
                );
            } )+
        }};
    }

    const DEMO_MANIFEST: &[u8] = br#"{
        "id": "demo",
        "displayName": "Demo",
        "provider": "demo.wasm",
        "defaultMount": "demo"
    }"#;

    const DEMO_MANIFEST_VERSIONED: &[u8] = br#"{
        "id": "demo",
        "displayName": "Demo",
        "provider": "demo.wasm",
        "defaultMount": "demo",
        "version": "0.3.1"
    }"#;

    const INVALID_MANIFEST_BAD_ID: &[u8] = br#"{
        "id": "bad id!",
        "displayName": "Bad",
        "provider": "bad.wasm",
        "defaultMount": "bad"
    }"#;

    const INVALID_MANIFEST_FRACTIONAL_MEMORY: &[u8] = br#"{
        "id": "bad",
        "displayName": "Bad",
        "provider": "bad.wasm",
        "defaultMount": "bad",
        "capabilities": [
            { "kind": "memoryMb", "value": 1.5, "why": "bad" }
        ]
    }"#;

    const INVALID_MANIFEST_OUT_OF_RANGE_MEMORY: &[u8] = br#"{
        "id": "bad",
        "displayName": "Bad",
        "provider": "bad.wasm",
        "defaultMount": "bad",
        "capabilities": [
            { "kind": "memoryMb", "value": 4294967296, "why": "bad" }
        ]
    }"#;

    const INVALID_MANIFEST_CONFIG_SCHEMA: &[u8] = br#"{
        "id": "bad",
        "displayName": "Bad",
        "provider": "bad.wasm",
        "defaultMount": "bad",
        "configSchema": { "type": 7 }
    }"#;

    const GUIDANCE_MANIFEST: &[u8] = br#"{
        "id": "demo",
        "displayName": "Demo",
        "provider": "demo.wasm",
        "defaultMount": "demo",
        "auth": {
            "inject": { "domains": ["api.demo.test"], "header": "Authorization", "prefix": "Bearer " },
            "default": "pat",
            "schemes": {
                "pat": {
                    "type": "staticToken",
                    "description": "Demo API token",
                    "creationUrl": "https://demo.test/settings/tokens",
                    "summary": "Paste a personal token",
                    "setup": ["Open settings", "Click create token"],
                    "docsUrl": "https://demo.test/docs/auth"
                }
            }
        }
    }"#;

    const OAUTH_TOKEN_ENDPOINT_MANIFEST: &[u8] = br#"{
        "id": "conf",
        "displayName": "Confidential",
        "provider": "conf.wasm",
        "defaultMount": "conf",
        "auth": {
            "inject": { "domains": ["api.conf.test"], "header": "Authorization", "prefix": "Bearer " },
            "default": "oauth",
            "schemes": {
                "oauth": {
                    "type": "oauth",
                    "displayName": "Conf OAuth",
                    "clientId": "abc",
                    "tokenEndpointAuth": "clientSecretPost",
                    "flow": {
                        "kind": "pkceLoopback",
                        "authorizationEndpoint": "https://conf.test/oauth/authorize",
                        "tokenEndpoint": "https://conf.test/oauth/token",
                        "redirectUriTemplate": "http://127.0.0.1:{port}/callback"
                    }
                }
            }
        }
    }"#;

    fn oauth_scheme_mut(manifest: &mut ProviderManifest) -> &mut OauthScheme {
        let AuthScheme::Oauth(oauth) = manifest
            .auth
            .as_mut()
            .expect("oauth auth")
            .schemes
            .get_mut("oauth")
            .expect("oauth scheme")
        else {
            panic!("expected oauth scheme");
        };
        oauth
    }

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
    fn checked_in_provider_manifest_contract() {
        let schema_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("schema/omnifs.provider.schema.json");
        let checked_in: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(schema_path).unwrap()).unwrap();
        let generated = serde_json::to_value(crate::provider_manifest_json()).unwrap();
        assert_eq!(checked_in, generated);

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

        // arxiv and dns author their manifests from `#[provider]` annotations
        // and carry no `omnifs.provider.json`; the rest are still JSON-authored.
        assert_eq!(
            parsed,
            ["db", "docker", "github", "kubernetes", "linear", "oura"]
        );
    }

    #[test]
    fn provider_manifest_version_round_trips_and_is_optional() {
        let bare = ProviderManifest::from_bytes(DEMO_MANIFEST).unwrap();
        assert_eq!(bare.version, None);
        let reencoded = serde_json::to_value(&bare).unwrap();
        assert!(reencoded.get("version").is_none());

        let stamped = ProviderManifest::from_bytes(DEMO_MANIFEST_VERSIONED).unwrap();
        assert_eq!(stamped.version.as_deref(), Some("0.3.1"));
        let reencoded = serde_json::to_value(&stamped).unwrap();
        assert_eq!(reencoded["version"], "0.3.1");
    }

    #[test]
    fn provider_manifest_parse_rejects_invalid_input() {
        cases!(
            (
                "non-slug id",
                INVALID_MANIFEST_BAD_ID,
                |error: &ProviderMetadataError| {
                    matches!(
                        error,
                        ProviderMetadataError::Validation(message) if message.contains("bad id!")
                    )
                }
            ),
            (
                "fractional memory capability",
                INVALID_MANIFEST_FRACTIONAL_MEMORY,
                |error: &ProviderMetadataError| {
                    matches!(
                        error,
                        ProviderMetadataError::Schema(_) | ProviderMetadataError::Json(_)
                    )
                }
            ),
            (
                "out-of-range memory capability",
                INVALID_MANIFEST_OUT_OF_RANGE_MEMORY,
                |error: &ProviderMetadataError| {
                    matches!(
                        error,
                        ProviderMetadataError::Schema(_) | ProviderMetadataError::Json(_)
                    )
                }
            ),
            (
                "invalid config schema",
                INVALID_MANIFEST_CONFIG_SCHEMA,
                |error: &ProviderMetadataError| {
                    matches!(
                        error,
                        ProviderMetadataError::Validation(message) if message.contains("configSchema")
                    )
                }
            ),
        );
    }

    #[test]
    fn provider_metadata_rejects_invalid_oauth_surface() {
        reject_oauth_surface!(
            ("authorizationEndpoint", |manifest| {
                oauth_scheme_mut(&mut manifest).authorization_endpoint =
                    "http://linear.app/oauth/authorize".to_string();
            }),
            ("{port}", |manifest| {
                oauth_scheme_mut(&mut manifest).flow =
                    OAuthFlow::PkceLoopback(PkceLoopbackConfig {
                        redirect_uri_template: "http://127.0.0.1/callback".to_string(),
                    });
            }),
            ("must not contain {port}", |manifest| {
                oauth_scheme_mut(&mut manifest).flow =
                    OAuthFlow::PkceManualCode(PkceManualCodeConfig {
                        redirect_uri: "https://example.com/callback/{port}".to_string(),
                    });
            }),
            ("auth.inject.domains", |manifest| {
                manifest.auth.as_mut().expect("oauth auth").inject.domains =
                    vec!["https://api.linear.app".to_string()];
            }),
            ("auth.inject.domains", |manifest| {
                manifest.auth.as_mut().expect("oauth auth").inject.domains =
                    vec!["*.linear.app".to_string()];
            }),
        );

        let mut accepted = oauth_provider_manifest();
        oauth_scheme_mut(&mut accepted).flow = OAuthFlow::ClientSideToken(ClientSideTokenConfig {
            redirect_uri_template: "http://localhost:58880".to_string(),
        });
        encode_provider_manifest(&accepted).unwrap();

        let mut rejected = oauth_provider_manifest();
        oauth_scheme_mut(&mut rejected).flow = OAuthFlow::ClientSideToken(ClientSideTokenConfig {
            redirect_uri_template: "https://example.com/callback".to_string(),
        });
        let error = encode_provider_manifest(&rejected).unwrap_err();
        assert!(
            matches!(error, ProviderMetadataError::Validation(message) if message.contains("http://localhost:<port>"))
        );
    }

    #[test]
    fn provider_manifest_auth_wire_shapes() {
        let guidance_manifest = ProviderManifest::from_bytes(GUIDANCE_MANIFEST).unwrap();
        let auth = guidance_manifest.auth.as_ref().expect("auth");
        let guidance = auth.guidance_for("pat");
        assert_eq!(guidance.summary.as_deref(), Some("Paste a personal token"));
        assert_eq!(
            guidance.setup_steps,
            ["Open settings", "Click create token"]
        );
        assert_eq!(
            guidance.docs_url.as_deref(),
            Some("https://demo.test/docs/auth")
        );
        let reparsed = encode_provider_manifest(&guidance_manifest).unwrap();
        assert_eq!(reparsed.auth.unwrap().guidance, auth.guidance);

        let oauth_manifest = ProviderManifest::from_bytes(OAUTH_TOKEN_ENDPOINT_MANIFEST).unwrap();
        let method = |manifest: &ProviderManifest| match &manifest.auth.as_ref().unwrap().schemes["oauth"]
        {
            AuthScheme::Oauth(oauth) => oauth.token_endpoint_auth,
            other => panic!("expected oauth scheme, got {other:?}"),
        };
        assert_eq!(
            method(&oauth_manifest),
            TokenEndpointAuthMethod::ClientSecretPost
        );
        let reparsed = encode_provider_manifest(&oauth_manifest).unwrap();
        assert_eq!(method(&reparsed), TokenEndpointAuthMethod::ClientSecretPost);
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
    fn byo_oauth_scheme_without_client_id_requires_guidance() {
        let json = serde_json::json!({
            "id": "byo",
            "displayName": "BYO",
            "provider": "byo.wasm",
            "defaultMount": "byo",
            "auth": {
                "inject": { "domains": ["api.byo.test"], "header": "Authorization", "prefix": "Bearer " },
                "default": "oauth",
                "schemes": {
                    "oauth": {
                        "type": "oauth",
                        "displayName": "BYO OAuth",
                        "flow": {
                            "kind": "pkceLoopback",
                            "authorizationEndpoint": "https://byo.test/oauth/authorize",
                            "tokenEndpoint": "https://byo.test/oauth/token",
                            "redirectUriTemplate": "http://127.0.0.1:{port}/callback"
                        }
                    }
                }
            }
        });
        let bytes = serde_json::to_vec(&json).unwrap();
        let error = ProviderManifest::from_bytes(&bytes).unwrap_err();
        assert!(
            matches!(&error, ProviderMetadataError::Validation(message) if message.contains("ships no clientId")),
            "unexpected error: {error}"
        );

        // Adding setup steps satisfies the rule.
        let mut json = json;
        json["auth"]["schemes"]["oauth"]["setup"] =
            serde_json::json!(["Create an OAuth app at https://byo.test/apps"]);
        let bytes = serde_json::to_vec(&json).unwrap();
        ProviderManifest::from_bytes(&bytes).expect("guidance satisfies BYO rule");
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
