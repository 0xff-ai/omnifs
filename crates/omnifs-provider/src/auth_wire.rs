use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AuthManifest {
    pub schemes: Vec<AuthScheme>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::large_enum_variant)]
pub enum AuthScheme {
    None,
    StaticToken(StaticTokenScheme),
    Oauth(OauthScheme),
}

impl AuthScheme {
    /// The scheme's key, or `None` for [`AuthScheme::None`].
    #[must_use]
    pub fn key(&self) -> Option<&str> {
        match self {
            AuthScheme::None => None,
            AuthScheme::StaticToken(scheme) => Some(&scheme.key),
            AuthScheme::Oauth(scheme) => Some(&scheme.key),
        }
    }

    /// Hostnames this scheme's credential is injected into. Empty for
    /// [`AuthScheme::None`].
    #[must_use]
    pub fn inject_domains(&self) -> &[String] {
        match self {
            AuthScheme::None => &[],
            AuthScheme::StaticToken(scheme) => &scheme.inject_domains,
            AuthScheme::Oauth(scheme) => &scheme.inject_domains,
        }
    }
}

/// Human-facing setup guidance for a single auth scheme.
///
/// Display metadata only: it never affects header injection, so it rides on the
/// manifest [`ProviderAuthManifest`](crate::ProviderAuthManifest) rather than on
/// the injection-facing [`AuthScheme`]. The host pairs it with its own canned
/// per-flow-kind explanation; a provider supplies only what is specific to it
/// (e.g. "create an OAuth app", "enable the Calendar API").
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SchemeGuidance {
    /// One-line summary shown in scheme pickers during `omnifs init`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Ordered provider-specific prerequisite steps, rendered after the host's
    /// canned flow-kind explanation. Required for an OAuth scheme that ships no
    /// client id (the user must create their own app).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub setup_steps: Vec<String>,
    /// Link to provider documentation for this auth path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docs_url: Option<String>,
}

impl SchemeGuidance {
    /// Whether the provider supplied any setup guidance at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.summary.is_none() && self.setup_steps.is_empty() && self.docs_url.is_none()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StaticTokenScheme {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header_name: Option<String>,
    pub value_prefix: String,
    pub description: String,
    pub inject_domains: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creation_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation: Option<TokenValidation>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OauthScheme {
    pub key: String,
    pub display_name: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revocation_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_scopes: Vec<String>,
    pub flow: OAuthFlow,
    #[serde(default, skip_serializing_if = "TokenEndpointAuthMethod::is_none")]
    pub token_endpoint_auth: TokenEndpointAuthMethod,
    #[serde(default)]
    pub refresh_token_rotates: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_authorize_params: Vec<KeyValue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_token_params: Vec<KeyValue>,
    pub inject_domains: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inject_header_name: Option<String>,
    pub inject_value_prefix: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::upper_case_acronyms)]
pub enum OAuthFlow {
    PkceLoopback(PkceLoopbackConfig),
    PkceManualCode(PkceManualCodeConfig),
    ClientSideToken(ClientSideTokenConfig),
    DeviceCode(DeviceCodeConfig),
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PkceLoopbackConfig {
    pub redirect_uri_template: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PkceManualCodeConfig {
    pub redirect_uri: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ClientSideTokenConfig {
    pub redirect_uri_template: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DeviceCodeConfig {
    pub device_authorization_endpoint: String,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum TokenEndpointAuthMethod {
    /// Public client: no secret presented at the token endpoint (PKCE).
    #[default]
    None,
    /// Confidential client: secret sent in the token request body.
    ClientSecretPost,
    /// Confidential client: secret sent via HTTP Basic auth.
    ClientSecretBasic,
}

impl TokenEndpointAuthMethod {
    /// Whether this is the public-client default (no secret presented).
    #[must_use]
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct KeyValue {
    pub key: String,
    pub value: String,
}

/// Token self-validation probe, carried by a provider's static-token scheme.
///
/// The host uses this to verify a newly-entered token before storing it.
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
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub extract: std::collections::BTreeMap<String, String>,
}
