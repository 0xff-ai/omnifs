use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthManifest {
    pub schemes: Vec<AuthScheme>,
}

impl AuthManifest {
    /// Ambient credential sources declared across this manifest's static-token
    /// schemes, in declaration order. `omnifs init` interprets these to offer an
    /// existing token for import.
    pub fn ambient_sources(&self) -> impl Iterator<Item = &AmbientSource> {
        self.schemes
            .iter()
            .filter_map(|scheme| match scheme {
                AuthScheme::StaticToken(scheme) => Some(scheme.ambient_sources.iter()),
                _ => None,
            })
            .flatten()
    }
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
    /// Places the host may find a token the user already has, so `omnifs init`
    /// can offer to import it instead of starting a fresh flow. Each source is
    /// an environment variable or a command to run; the host reads them
    /// generically, with no provider name baked into the host.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ambient_sources: Vec<AmbientSource>,
}

/// A place the host can look for a token the user already has for this scheme.
///
/// Declared by the provider and interpreted generically by the host: the host
/// never special-cases a provider by name. The `note` is human-facing text
/// shown next to the detected credential during `omnifs init`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AmbientSource {
    pub kind: AmbientKind,
    /// Human-facing description of where this credential comes from.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

/// The mechanism a host uses to read an ambient credential.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AmbientKind {
    /// Read the token from an environment variable.
    EnvVar { name: String },
    /// Run a command (argv exec, never a shell) and take its trimmed stdout as
    /// the token.
    Command { argv: Vec<String> },
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PkceLoopbackConfig {
    pub redirect_uri_template: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PkceManualCodeConfig {
    pub redirect_uri: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClientSideTokenConfig {
    pub redirect_uri_template: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeviceCodeConfig {
    pub device_authorization_endpoint: String,
    /// How this provider's token endpoint signals a still-pending authorization
    /// while the user approves the device. Declares whether the host must apply
    /// the pending-in-OK-body compatibility rewrite. Defaults to RFC 8628.
    #[serde(default)]
    pub device_poll_compat: DevicePollCompat,
}

/// How a device-code token endpoint reports a still-pending authorization.
///
/// RFC 8628 mandates a 4xx response with an `error` body while the user is
/// still approving. Some providers instead return `200 OK` with the same error
/// body; the host rewrites those to a 4xx so the poll loop keeps waiting. The
/// provider declares which behavior its token endpoint exhibits.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DevicePollCompat {
    /// Conformant: the token endpoint returns a 4xx status with an error body
    /// while pending. No host rewrite is applied.
    #[default]
    Rfc8628,
    /// Non-conformant: the token endpoint returns `200 OK` with an error JSON
    /// body while pending. The host rewrites such responses to `400` so the
    /// poll loop treats `authorization_pending`/`slow_down` as continue signals.
    ErrorInOkBody,
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
