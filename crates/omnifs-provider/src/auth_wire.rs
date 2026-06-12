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

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StaticTokenScheme {
    pub key: String,
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
    pub revocation_endpoint: Option<String>,
    pub default_client_id: Option<String>,
    pub default_scopes: Vec<String>,
    pub flow: OAuthFlow,
    pub token_endpoint_auth: TokenEndpointAuthMethod,
    pub refresh_token_rotates: bool,
    pub extra_authorize_params: Vec<KeyValue>,
    pub extra_token_params: Vec<KeyValue>,
    pub inject_domains: Vec<String>,
    pub inject_header_name: Option<String>,
    pub inject_value_prefix: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::upper_case_acronyms)]
pub enum OAuthFlow {
    PkceLoopback(PkceLoopbackConfig),
    PkceManualCode(PkceManualCodeConfig),
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
pub struct DeviceCodeConfig {
    pub device_authorization_endpoint: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum TokenEndpointAuthMethod {
    None,
    ClientSecretPost,
    ClientSecretBasic,
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
