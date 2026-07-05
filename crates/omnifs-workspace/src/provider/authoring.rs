//! Ergonomic constructors for authoring provider metadata.
//!
//! A provider builds its [`ProviderManifest`](crate::provider::ProviderManifest) (via the
//! `#[provider]` macro) and auth block directly in these owned wire types, and
//! the harvester serializes the result verbatim into the
//! `omnifs.provider-metadata.v1` section. There is no second representation and
//! no translation step: what a provider constructs here is exactly the wire
//! shape the host reads back. These builders only add construction sugar; they
//! never change a wire shape.

use std::collections::BTreeMap;

use crate::authn::scheme::{
    AmbientKind, AmbientSource, AuthScheme, ClientSideTokenConfig, DeviceCodeConfig,
    DevicePollCompat, OAuthFlow, OauthScheme, PkceLoopbackConfig, SchemeGuidance,
    StaticTokenScheme, TokenEndpointAuthMethod, TokenValidation,
};
use crate::provider::manifest::ProviderAuthManifest;

fn strings<I, S>(items: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    items.into_iter().map(Into::into).collect()
}

impl ProviderAuthManifest {
    /// Start an auth manifest. `default` names the scheme `omnifs init` picks
    /// when the user makes no explicit choice.
    #[must_use]
    pub fn builder(default: impl Into<String>) -> ProviderAuthBuilder {
        ProviderAuthBuilder {
            default: default.into(),
            schemes: Vec::new(),
            guidance: BTreeMap::new(),
        }
    }
}

/// Accumulates schemes and their setup guidance into a [`ProviderAuthManifest`].
/// Guidance is display-only, so it rides in the manifest's guidance map keyed by
/// scheme key, not on the injection-facing scheme.
pub struct ProviderAuthBuilder {
    default: String,
    schemes: Vec<AuthScheme>,
    guidance: BTreeMap<String, SchemeGuidance>,
}

impl ProviderAuthBuilder {
    /// Add a static-token scheme with its setup guidance.
    #[must_use]
    pub fn static_token(mut self, scheme: StaticTokenScheme, guidance: SchemeGuidance) -> Self {
        if !guidance.is_empty() {
            self.guidance.insert(scheme.key.clone(), guidance);
        }
        self.schemes.push(AuthScheme::StaticToken(scheme));
        self
    }

    /// Add an OAuth scheme with its setup guidance.
    #[must_use]
    pub fn oauth(mut self, scheme: OauthScheme, guidance: SchemeGuidance) -> Self {
        if !guidance.is_empty() {
            self.guidance.insert(scheme.key.clone(), guidance);
        }
        self.schemes.push(AuthScheme::Oauth(scheme));
        self
    }

    #[must_use]
    pub fn build(self) -> ProviderAuthManifest {
        ProviderAuthManifest {
            default: self.default,
            schemes: self.schemes,
            guidance: self.guidance,
        }
    }
}

impl StaticTokenScheme {
    /// A bring-your-own static token. Defaults to the `Authorization` header and
    /// a `Bearer ` value prefix; the host treats a missing header name as
    /// `Authorization`.
    #[must_use]
    pub fn new(key: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            header_name: None,
            value_prefix: "Bearer ".to_string(),
            description: description.into(),
            inject_domains: Vec::new(),
            creation_url: None,
            validation: None,
            ambient_sources: Vec::new(),
        }
    }

    /// Hostnames the host injects this token into. Required; a scheme that
    /// injects nowhere can never authenticate a request.
    #[must_use]
    pub fn inject<I, S>(mut self, domains: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.inject_domains = strings(domains);
        self
    }

    /// Override the value prefix (default `Bearer `). Pass `""` for a raw token.
    #[must_use]
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.value_prefix = prefix.into();
        self
    }

    #[must_use]
    pub fn creation_url(mut self, url: impl Into<String>) -> Self {
        self.creation_url = Some(url.into());
        self
    }

    #[must_use]
    pub fn validation(mut self, validation: TokenValidation) -> Self {
        self.validation = Some(validation);
        self
    }

    /// Declare where the host may find a token the user already has, so
    /// `omnifs init` can offer to import it instead of starting a fresh flow.
    #[must_use]
    pub fn ambient<I>(mut self, sources: I) -> Self
    where
        I: IntoIterator<Item = AmbientSource>,
    {
        self.ambient_sources = sources.into_iter().collect();
        self
    }
}

impl AmbientSource {
    /// An ambient source read from an environment variable.
    #[must_use]
    pub fn env_var(name: impl Into<String>) -> Self {
        Self {
            kind: AmbientKind::EnvVar { name: name.into() },
            note: String::new(),
        }
    }

    /// An ambient source read from a command's trimmed stdout. Declares argv
    /// only; the host execs it directly, never through a shell.
    #[must_use]
    pub fn command<I, S>(argv: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            kind: AmbientKind::Command {
                argv: strings(argv),
            },
            note: String::new(),
        }
    }

    /// Human-facing description shown next to the detected credential during
    /// `omnifs init`.
    #[must_use]
    pub fn note(mut self, note: impl Into<String>) -> Self {
        self.note = note.into();
        self
    }
}

impl OauthScheme {
    /// Start an OAuth scheme with the device-code flow.
    #[must_use]
    pub fn device_code(
        key: impl Into<String>,
        display_name: impl Into<String>,
        authorization_endpoint: impl Into<String>,
        device_authorization_endpoint: impl Into<String>,
        token_endpoint: impl Into<String>,
    ) -> Self {
        Self::with_flow(
            key,
            display_name,
            authorization_endpoint,
            token_endpoint,
            OAuthFlow::DeviceCode(DeviceCodeConfig {
                device_authorization_endpoint: device_authorization_endpoint.into(),
                device_poll_compat: DevicePollCompat::Rfc8628,
            }),
        )
    }

    /// Start an OAuth scheme with the PKCE loopback flow.
    #[must_use]
    pub fn pkce_loopback(
        key: impl Into<String>,
        display_name: impl Into<String>,
        authorization_endpoint: impl Into<String>,
        token_endpoint: impl Into<String>,
        redirect_uri_template: impl Into<String>,
    ) -> Self {
        Self::with_flow(
            key,
            display_name,
            authorization_endpoint,
            token_endpoint,
            OAuthFlow::PkceLoopback(PkceLoopbackConfig {
                redirect_uri_template: redirect_uri_template.into(),
            }),
        )
    }

    /// Start an OAuth scheme with the client-side-token flow.
    #[must_use]
    pub fn client_side_token(
        key: impl Into<String>,
        display_name: impl Into<String>,
        authorization_endpoint: impl Into<String>,
        token_endpoint: impl Into<String>,
        redirect_uri_template: impl Into<String>,
    ) -> Self {
        Self::with_flow(
            key,
            display_name,
            authorization_endpoint,
            token_endpoint,
            OAuthFlow::ClientSideToken(ClientSideTokenConfig {
                redirect_uri_template: redirect_uri_template.into(),
            }),
        )
    }

    fn with_flow(
        key: impl Into<String>,
        display_name: impl Into<String>,
        authorization_endpoint: impl Into<String>,
        token_endpoint: impl Into<String>,
        flow: OAuthFlow,
    ) -> Self {
        let refresh_token_rotates = matches!(flow, OAuthFlow::PkceLoopback(_));
        Self {
            key: key.into(),
            display_name: display_name.into(),
            authorization_endpoint: authorization_endpoint.into(),
            token_endpoint: token_endpoint.into(),
            revocation_endpoint: None,
            default_client_id: None,
            default_scopes: Vec::new(),
            flow,
            token_endpoint_auth: TokenEndpointAuthMethod::None,
            refresh_token_rotates,
            extra_authorize_params: Vec::new(),
            extra_token_params: Vec::new(),
            inject_domains: Vec::new(),
            inject_header_name: None,
            inject_value_prefix: "Bearer ".to_string(),
        }
    }

    /// Hostnames the host injects the obtained token into. Required.
    #[must_use]
    pub fn inject<I, S>(mut self, domains: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.inject_domains = strings(domains);
        self
    }

    /// Override the value prefix (default `Bearer `).
    #[must_use]
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.inject_value_prefix = prefix.into();
        self
    }

    #[must_use]
    pub fn client_id(mut self, client_id: impl Into<String>) -> Self {
        self.default_client_id = Some(client_id.into());
        self
    }

    #[must_use]
    pub fn scopes<I, S>(mut self, scopes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.default_scopes = strings(scopes);
        self
    }

    /// Declare how this scheme's device-code token endpoint signals a pending
    /// authorization. Only a device-code flow has this behavior, so this is a
    /// no-op on any other flow.
    #[must_use]
    pub fn device_poll_compat(mut self, compat: DevicePollCompat) -> Self {
        if let OAuthFlow::DeviceCode(config) = &mut self.flow {
            config.device_poll_compat = compat;
        }
        self
    }
}

impl TokenValidation {
    #[must_use]
    pub fn get(url: impl Into<String>) -> Self {
        Self::probe("GET", url, None)
    }

    #[must_use]
    pub fn post(url: impl Into<String>, body: impl Into<String>) -> Self {
        Self::probe("POST", url, Some(body.into()))
    }

    fn probe(method: impl Into<String>, url: impl Into<String>, body: Option<String>) -> Self {
        Self {
            method: method.into(),
            url: url.into(),
            body,
            expect_status: 200,
            json_pointer: None,
            extract: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn json_pointer(mut self, pointer: impl Into<String>) -> Self {
        self.json_pointer = Some(pointer.into());
        self
    }

    /// Identity fields to extract from the validation response, as
    /// `(key, json-pointer)` pairs.
    #[must_use]
    pub fn extract<I, K, V>(mut self, entries: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        self.extract = entries
            .into_iter()
            .map(|(key, pointer)| (key.into(), pointer.into()))
            .collect();
        self
    }
}

impl SchemeGuidance {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
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
        self.setup_steps = strings(steps);
        self
    }

    #[must_use]
    pub fn docs_url(mut self, url: impl Into<String>) -> Self {
        self.docs_url = Some(url.into());
        self
    }
}
