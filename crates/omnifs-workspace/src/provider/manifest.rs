//! Typed provider manifest embedded in the `omnifs.provider-metadata.v1` wasm custom section.

use crate::authn::scheme::{AuthManifest, AuthScheme, OAuthFlow, OauthScheme, SchemeGuidance};
use crate::provider::config::ConfigMetadata;
use crate::provider::sections::{
    ProviderMetadataError, is_hostname_only, validate_provider_manifest,
};
use omnifs_caps::{Grants, Need, domain_matches};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<Need>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<ProviderAuthManifest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<ConfigMetadata>,
}

/// Provider auth block from the `omnifs.provider-metadata.v1` embedded section.
///
/// Each scheme is self-contained: it carries its own injection domains, header,
/// and prefix, so the embedded wire form is exactly what the host and
/// `omnifs-auth` consume. There is no compact-vs-expanded encoding and no
/// transform on read, the provider serializes [`AuthScheme`] directly.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderAuthManifest {
    /// Key of the scheme `omnifs init` defaults to when the user makes no choice.
    pub default: String,
    /// The schemes a user can pick, each self-contained with its own injection
    /// domains, header, and prefix.
    pub schemes: Vec<AuthScheme>,
    /// Per-scheme setup guidance, keyed by scheme key. Display metadata only;
    /// never affects header injection, so it rides here rather than on the
    /// injection-facing [`AuthScheme`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub guidance: BTreeMap<String, SchemeGuidance>,
}

impl ProviderAuthManifest {
    /// The scheme registered under `key`, if any.
    #[must_use]
    pub fn scheme(&self, key: &str) -> Option<&AuthScheme> {
        self.schemes.iter().find(|scheme| scheme.key() == Some(key))
    }

    #[must_use]
    pub fn default_scheme(&self) -> Option<(&str, &AuthScheme)> {
        let scheme = self.scheme(&self.default)?;
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
        AuthManifest {
            schemes: self.schemes.clone(),
        }
    }

    fn validate(&self) -> Result<(), ProviderMetadataError> {
        validate_non_empty("auth.default", &self.default)?;
        if self.scheme(&self.default).is_none() {
            return Err(ProviderMetadataError::Validation(format!(
                "auth.default {:?} has no matching auth.schemes entry",
                self.default
            )));
        }
        let mut seen = HashSet::new();
        for scheme in &self.schemes {
            let Some(key) = scheme.key() else {
                return Err(ProviderMetadataError::Validation(
                    "auth.schemes contains a `none` scheme, which a provider cannot declare"
                        .to_string(),
                ));
            };
            if !seen.insert(key) {
                return Err(ProviderMetadataError::Validation(format!(
                    "auth.schemes: duplicate scheme key {key:?}"
                )));
            }
            validate_scheme(key, scheme)?;
            // Each scheme must inject its credential somewhere: the host keys
            // injection entirely off per-scheme domains, so an empty list ships a
            // credential that is silently never attached to any request.
            if scheme.inject_domains().is_empty() {
                return Err(ProviderMetadataError::Validation(format!(
                    "auth.schemes.{key}: declares no inject domains; call `.inject(&[..])`"
                )));
            }
            validate_inject_domains(scheme.inject_domains())?;
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

impl ProviderManifest {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ProviderMetadataError> {
        let value: serde_json::Value =
            serde_json::from_slice(bytes).map_err(ProviderMetadataError::Json)?;
        validate_provider_manifest(&value)?;
        let manifest: Self = serde_json::from_value(value).map_err(ProviderMetadataError::Json)?;
        manifest.validate()?;
        Ok(manifest)
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
        crate::ids::ProviderName::new(self.id.as_str())
            .map_err(|error| ProviderMetadataError::Validation(error.to_string()))?;
        validate_non_empty("displayName", &self.display_name)?;
        validate_non_empty("provider", &self.provider)?;
        validate_non_empty("defaultMount", &self.default_mount)?;
        for entry in &self.capabilities {
            validate_non_empty("capabilities.why", entry.why())?;
            // A dynamic grant is resolved at mount-start from a config field
            // bound to the matching host resource: a unix socket into the
            // callout allowlist, a preopened path into a WASI preopen. Any
            // other dynamic kind has no resolver and would resolve to an empty
            // allowlist, denying the provider at its first callout. Reject
            // those at the manifest boundary.
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
            self.validate_auth_inject_domain_coverage(auth)?;
        }
        if let Some(config) = self.config.as_ref() {
            config.validate()?;
        }
        Ok(())
    }

    fn validate_auth_inject_domain_coverage(
        &self,
        auth: &ProviderAuthManifest,
    ) -> Result<(), ProviderMetadataError> {
        for scheme in &auth.schemes {
            let Some(key) = scheme.key() else {
                continue;
            };
            for domain in scheme.inject_domains() {
                let covered = self.capabilities.iter().any(|need| {
                    matches!(
                        need,
                        Need::Domain {
                            value,
                            dynamic: false,
                            ..
                        } if domain_matches(value, domain)
                    )
                });
                if !covered {
                    return Err(ProviderMetadataError::Validation(format!(
                        "auth.schemes.{key}.injectDomains entry {domain:?} is not covered by a declared domain capability need"
                    )));
                }
            }
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

fn validate_scheme(key: &str, scheme: &AuthScheme) -> Result<(), ProviderMetadataError> {
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
    use crate::authn::scheme::{
        ClientSideTokenConfig, PkceLoopbackConfig, PkceManualCodeConfig, TokenEndpointAuthMethod,
    };
    use crate::provider::sections::ProviderMetadataError;
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

    const INVALID_MANIFEST_CONFIG: &[u8] = br#"{
        "id": "bad",
        "displayName": "Bad",
        "provider": "bad.wasm",
        "defaultMount": "bad",
        "config": {
            "fields": [
                {
                    "name": "endpoint",
                    "type": { "kind": "integer" },
                    "binding": { "kind": "socket" }
                }
            ]
        }
    }"#;

    const GUIDANCE_MANIFEST: &[u8] = br#"{
        "id": "demo",
        "displayName": "Demo",
        "provider": "demo.wasm",
        "defaultMount": "demo",
        "capabilities": [
            { "kind": "domain", "value": "api.demo.test", "why": "Fetch Demo API resources." }
        ],
        "auth": {
            "default": "pat",
            "schemes": [
                {
                    "staticToken": {
                        "key": "pat",
                        "valuePrefix": "Bearer ",
                        "description": "Demo API token",
                        "injectDomains": ["api.demo.test"],
                        "creationUrl": "https://demo.test/settings/tokens"
                    }
                }
            ],
            "guidance": {
                "pat": {
                    "summary": "Paste a personal token",
                    "setupSteps": ["Open settings", "Click create token"],
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
        "capabilities": [
            { "kind": "domain", "value": "api.conf.test", "why": "Fetch confidential API resources." }
        ],
        "auth": {
            "default": "oauth",
            "schemes": [
                {
                    "oauth": {
                        "key": "oauth",
                        "displayName": "Conf OAuth",
                        "authorizationEndpoint": "https://conf.test/oauth/authorize",
                        "tokenEndpoint": "https://conf.test/oauth/token",
                        "defaultClientId": "abc",
                        "tokenEndpointAuth": "clientSecretPost",
                        "flow": {
                            "pkceLoopback": {
                                "redirectUriTemplate": "http://127.0.0.1:{port}/callback"
                            }
                        },
                        "injectDomains": ["api.conf.test"],
                        "injectValuePrefix": "Bearer "
                    }
                }
            ]
        }
    }"#;

    fn oauth_scheme_mut(manifest: &mut ProviderManifest) -> &mut OauthScheme {
        let scheme = manifest
            .auth
            .as_mut()
            .expect("oauth auth")
            .schemes
            .iter_mut()
            .find(|scheme| matches!(scheme, AuthScheme::Oauth(_)))
            .expect("oauth scheme");
        let AuthScheme::Oauth(oauth) = scheme else {
            panic!("expected oauth scheme");
        };
        oauth
    }

    #[test]
    fn checked_in_provider_manifest_schema_matches_model() {
        // Every provider now authors its manifest from `#[provider]` annotations
        // (no `omnifs.provider.json`), so the checked-in JSON Schema is a pure
        // drift guard for the `ProviderManifest` model. Provider auth blocks are
        // exercised end-to-end by the `all_providers_initialize_and_seal` host
        // integration test, which loads every embedded manifest.
        let schema_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("schema/omnifs.provider.schema.json");
        let checked_in: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(schema_path).unwrap()).unwrap();
        let generated = serde_json::to_value(crate::provider::provider_manifest_json()).unwrap();
        assert_eq!(checked_in, generated);
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
                "invalid config metadata",
                INVALID_MANIFEST_CONFIG,
                |error: &ProviderMetadataError| {
                    matches!(
                        error,
                        ProviderMetadataError::Validation(message) if message.contains("host-resource bindings")
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
                oauth_scheme_mut(&mut manifest).inject_domains =
                    vec!["https://api.linear.app".to_string()];
            }),
            ("auth.inject.domains", |manifest| {
                oauth_scheme_mut(&mut manifest).inject_domains = vec!["*.linear.app".to_string()];
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
    fn provider_metadata_rejects_uncovered_inject_domains() {
        let mut manifest = oauth_provider_manifest();
        manifest.capabilities.clear();
        let error = encode_provider_manifest(&manifest).unwrap_err();
        assert!(
            matches!(
                &error,
                ProviderMetadataError::Validation(message)
                    if message.contains("auth.schemes.oauth")
                        && message.contains("api.linear.app")
                        && message.contains("domain capability need")
            ),
            "unexpected error: {error}"
        );

        let mut wildcard = oauth_provider_manifest();
        let Need::Domain { value, .. } = &mut wildcard.capabilities[0] else {
            panic!("oauth fixture starts with a domain need");
        };
        *value = "*".to_string();
        encode_provider_manifest(&wildcard).expect("wildcard domain need covers inject domain");
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
        let method = |manifest: &ProviderManifest| match manifest
            .auth
            .as_ref()
            .unwrap()
            .scheme("oauth")
            .expect("oauth scheme")
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
    fn byo_oauth_scheme_without_client_id_requires_guidance() {
        let json = serde_json::json!({
            "id": "byo",
            "displayName": "BYO",
            "provider": "byo.wasm",
            "defaultMount": "byo",
            "capabilities": [
                { "kind": "domain", "value": "api.byo.test", "why": "Fetch BYO API resources." }
            ],
            "auth": {
                "default": "oauth",
                "schemes": [
                    {
                        "oauth": {
                            "key": "oauth",
                            "displayName": "BYO OAuth",
                            "authorizationEndpoint": "https://byo.test/oauth/authorize",
                            "tokenEndpoint": "https://byo.test/oauth/token",
                            "flow": {
                                "pkceLoopback": {
                                    "redirectUriTemplate": "http://127.0.0.1:{port}/callback"
                                }
                            },
                            "injectDomains": ["api.byo.test"],
                            "injectValuePrefix": "Bearer "
                        }
                    }
                ]
            }
        });
        let bytes = serde_json::to_vec(&json).unwrap();
        let error = ProviderManifest::from_bytes(&bytes).unwrap_err();
        assert!(
            matches!(&error, ProviderMetadataError::Validation(message) if message.contains("ships no clientId")),
            "unexpected error: {error}"
        );

        // Adding setup guidance for the scheme satisfies the rule.
        let mut json = json;
        json["auth"]["guidance"] = serde_json::json!({
            "oauth": { "setupSteps": ["Create an OAuth app at https://byo.test/apps"] }
        });
        let bytes = serde_json::to_vec(&json).unwrap();
        ProviderManifest::from_bytes(&bytes).expect("guidance satisfies BYO rule");
    }

    fn oauth_provider_manifest() -> ProviderManifest {
        let json = serde_json::json!({
            "id": "linear",
            "displayName": "Linear",
            "provider": "omnifs_provider_linear.wasm",
            "defaultMount": "linear",
            "capabilities": [
                {
                    "kind": "domain",
                    "value": "api.linear.app",
                    "why": "Fetch Linear GraphQL resources."
                }
            ],
            "auth": {
                "default": "oauth",
                "schemes": [
                    {
                        "oauth": {
                            "key": "oauth",
                            "displayName": "Linear OAuth",
                            "authorizationEndpoint": "https://linear.app/oauth/authorize",
                            "tokenEndpoint": "https://api.linear.app/oauth/token",
                            "defaultClientId": "client-id",
                            "defaultScopes": ["read"],
                            "flow": {
                                "pkceLoopback": {
                                    "redirectUriTemplate": "http://127.0.0.1:{port}/callback"
                                }
                            },
                            "injectDomains": ["api.linear.app"],
                            "injectValuePrefix": "Bearer "
                        }
                    }
                ]
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
