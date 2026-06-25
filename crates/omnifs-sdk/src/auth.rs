//! Guest-side auth manifest builder.
//!
//! A provider declares its auth as a typed value referenced from
//! `#[provider(auth = path::to::value)]`; `manifest_json()` splices it into the
//! manifest's `auth` block at build. This module is Serialize-only and produces
//! exactly the compact wire form the host reads back (`ProviderAuthManifest`),
//! so the auth types the host owns stay host-side (they pull `wasmparser` and
//! `jsonschema`, which do not build for the guest).

use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};

/// A provider's auth manifest: how the host injects credentials, which scheme
/// `omnifs init` defaults to, and the schemes a user can pick.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Auth {
    inject: Inject,
    default: String,
    #[serde(serialize_with = "serialize_schemes")]
    schemes: Vec<(String, Scheme)>,
}

impl Auth {
    /// Start an auth manifest. `domains` are the hostnames the host injects the
    /// credential into; `default` names the scheme `omnifs init` picks when the
    /// user makes no explicit choice. Header defaults to `Authorization` with a
    /// `Bearer ` prefix; override with [`header`](Self::header) /
    /// [`prefix`](Self::prefix).
    pub fn new<D, S>(domains: D, default: impl Into<String>) -> Self
    where
        D: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            inject: Inject {
                domains: domains.into_iter().map(Into::into).collect(),
                header: "Authorization".to_string(),
                prefix: "Bearer ".to_string(),
            },
            default: default.into(),
            schemes: Vec::new(),
        }
    }

    #[must_use]
    pub fn header(mut self, header: impl Into<String>) -> Self {
        self.inject.header = header.into();
        self
    }

    #[must_use]
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.inject.prefix = prefix.into();
        self
    }

    /// Add a scheme under `key` (the identifier the user selects and the host
    /// stores the credential under).
    #[must_use]
    pub fn scheme(mut self, key: impl Into<String>, scheme: impl Into<Scheme>) -> Self {
        self.schemes.push((key.into(), scheme.into()));
        self
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Inject {
    domains: Vec<String>,
    header: String,
    prefix: String,
}

fn serialize_schemes<S: Serializer>(
    schemes: &[(String, Scheme)],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(Some(schemes.len()))?;
    for (key, scheme) in schemes {
        map.serialize_entry(key, scheme)?;
    }
    map.end()
}

/// One auth scheme: a user-supplied static token or a host-driven OAuth flow.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Scheme {
    StaticToken(StaticToken),
    Oauth(OAuth),
}

impl From<StaticToken> for Scheme {
    fn from(value: StaticToken) -> Self {
        Self::StaticToken(value)
    }
}

impl From<OAuth> for Scheme {
    fn from(value: OAuth) -> Self {
        Self::Oauth(value)
    }
}

/// A bring-your-own static token (personal access token / API key).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StaticToken {
    description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    creation_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    validation: Option<Validation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    setup: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    docs_url: Option<String>,
}

impl StaticToken {
    pub fn new(description: impl Into<String>) -> Self {
        Self {
            description: description.into(),
            creation_url: None,
            validation: None,
            summary: None,
            setup: Vec::new(),
            docs_url: None,
        }
    }

    #[must_use]
    pub fn creation_url(mut self, url: impl Into<String>) -> Self {
        self.creation_url = Some(url.into());
        self
    }

    #[must_use]
    pub fn validation(mut self, validation: Validation) -> Self {
        self.validation = Some(validation);
        self
    }

    #[must_use]
    pub fn summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = Some(summary.into());
        self
    }

    #[must_use]
    pub fn setup<I, S>(mut self, steps: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.setup = steps.into_iter().map(Into::into).collect();
        self
    }

    #[must_use]
    pub fn docs_url(mut self, url: impl Into<String>) -> Self {
        self.docs_url = Some(url.into());
        self
    }
}

/// A host-driven OAuth scheme. The flow is required; set it with one of
/// [`device_code`](Self::device_code), [`pkce_loopback`](Self::pkce_loopback),
/// or [`client_side_token`](Self::client_side_token).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuth {
    display_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    scopes: Vec<String>,
    flow: Flow,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    setup: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    docs_url: Option<String>,
}

impl OAuth {
    /// Start an OAuth scheme with the device-code flow.
    pub fn device_code(
        display_name: impl Into<String>,
        authorization_endpoint: impl Into<String>,
        device_authorization_endpoint: impl Into<String>,
        token_endpoint: impl Into<String>,
    ) -> Self {
        Self::with_flow(
            display_name,
            Flow::DeviceCode {
                authorization_endpoint: authorization_endpoint.into(),
                device_authorization_endpoint: device_authorization_endpoint.into(),
                token_endpoint: token_endpoint.into(),
            },
        )
    }

    /// Start an OAuth scheme with the PKCE loopback flow. The redirect template
    /// must contain `{port}`.
    pub fn pkce_loopback(
        display_name: impl Into<String>,
        authorization_endpoint: impl Into<String>,
        token_endpoint: impl Into<String>,
        redirect_uri_template: impl Into<String>,
    ) -> Self {
        Self::with_flow(
            display_name,
            Flow::PkceLoopback {
                authorization_endpoint: authorization_endpoint.into(),
                token_endpoint: token_endpoint.into(),
                redirect_uri_template: redirect_uri_template.into(),
            },
        )
    }

    /// Start an OAuth scheme with the client-side-token flow. The redirect must
    /// contain `{port}` or be a fixed `http://localhost:<port>` loopback.
    pub fn client_side_token(
        display_name: impl Into<String>,
        authorization_endpoint: impl Into<String>,
        token_endpoint: impl Into<String>,
        redirect_uri_template: impl Into<String>,
    ) -> Self {
        Self::with_flow(
            display_name,
            Flow::ClientSideToken {
                authorization_endpoint: authorization_endpoint.into(),
                token_endpoint: token_endpoint.into(),
                redirect_uri_template: redirect_uri_template.into(),
            },
        )
    }

    fn with_flow(display_name: impl Into<String>, flow: Flow) -> Self {
        Self {
            display_name: display_name.into(),
            client_id: None,
            scopes: Vec::new(),
            flow,
            summary: None,
            setup: Vec::new(),
            docs_url: None,
        }
    }

    #[must_use]
    pub fn client_id(mut self, client_id: impl Into<String>) -> Self {
        self.client_id = Some(client_id.into());
        self
    }

    #[must_use]
    pub fn scopes<I, S>(mut self, scopes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.scopes = scopes.into_iter().map(Into::into).collect();
        self
    }

    #[must_use]
    pub fn summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = Some(summary.into());
        self
    }

    #[must_use]
    pub fn setup<I, S>(mut self, steps: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.setup = steps.into_iter().map(Into::into).collect();
        self
    }

    #[must_use]
    pub fn docs_url(mut self, url: impl Into<String>) -> Self {
        self.docs_url = Some(url.into());
        self
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
#[serde(rename_all_fields = "camelCase")]
enum Flow {
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
    ClientSideToken {
        authorization_endpoint: String,
        token_endpoint: String,
        redirect_uri_template: String,
    },
}

/// A token-validation probe `omnifs init` runs to confirm a static token works
/// and extract identity fields for the credential record.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Validation {
    method: String,
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<String>,
    expect_status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    json_pointer: Option<String>,
    #[serde(
        skip_serializing_if = "Vec::is_empty",
        serialize_with = "serialize_extract"
    )]
    extract: Vec<(String, String)>,
}

fn serialize_extract<S: Serializer>(
    extract: &[(String, String)],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(Some(extract.len()))?;
    for (key, pointer) in extract {
        map.serialize_entry(key, pointer)?;
    }
    map.end()
}

impl Validation {
    pub fn get(url: impl Into<String>) -> Self {
        Self::new("GET", url)
    }

    pub fn post(url: impl Into<String>, body: impl Into<String>) -> Self {
        let mut validation = Self::new("POST", url);
        validation.body = Some(body.into());
        validation
    }

    fn new(method: &str, url: impl Into<String>) -> Self {
        Self {
            method: method.to_string(),
            url: url.into(),
            body: None,
            expect_status: 200,
            json_pointer: None,
            extract: Vec::new(),
        }
    }

    #[must_use]
    pub fn expect_status(mut self, status: u16) -> Self {
        self.expect_status = status;
        self
    }

    /// Require this JSON pointer to be present in the response body.
    #[must_use]
    pub fn json_pointer(mut self, pointer: impl Into<String>) -> Self {
        self.json_pointer = Some(pointer.into());
        self
    }

    /// Extract an identity field from the response body by JSON pointer, stored
    /// under `key` on the credential record.
    #[must_use]
    pub fn extract(mut self, key: impl Into<String>, pointer: impl Into<String>) -> Self {
        self.extract.push((key.into(), pointer.into()));
        self
    }
}
