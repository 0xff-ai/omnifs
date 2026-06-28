//! Guest-side provider auth metadata.
//!
//! These types are static-slice shaped so a provider can embed them in its
//! `Metadata` const. They carry no runtime behavior: the build-time harvester
//! converts them into the host's `ProviderManifest` and serializes that into the
//! `omnifs.provider-metadata.v1` section. Each scheme is self-contained, it owns
//! the domains/header/prefix the host injects its credential with.

/// A provider's auth manifest: which scheme `omnifs init` defaults to and the
/// schemes a user can pick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Auth {
    pub default: &'static str,
    pub schemes: &'static [SchemeEntry],
}

impl Auth {
    /// Start an auth manifest. `default` names the scheme `omnifs init` picks
    /// when the user makes no explicit choice.
    #[must_use]
    pub const fn new(default: &'static str, schemes: &'static [SchemeEntry]) -> Self {
        Self { default, schemes }
    }
}

/// One keyed auth scheme.
pub type SchemeEntry = (&'static str, Scheme);

/// How the host attaches this scheme's credential to outbound requests: the
/// hostnames it applies to, the header name, and the value prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inject {
    pub domains: &'static [&'static str],
    pub header: &'static str,
    pub prefix: &'static str,
}

impl Inject {
    const fn new() -> Self {
        Self {
            domains: &[],
            header: "Authorization",
            prefix: "Bearer ",
        }
    }
}

/// One auth scheme: a user-supplied static token or a host-driven OAuth flow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    StaticToken(StaticToken),
    Oauth(OAuth),
}

/// A bring-your-own static token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StaticToken {
    pub inject: Inject,
    pub description: &'static str,
    pub creation_url: Option<&'static str>,
    pub validation: Option<Validation>,
    pub summary: Option<&'static str>,
    pub setup: &'static [&'static str],
    pub docs_url: Option<&'static str>,
}

impl StaticToken {
    #[must_use]
    pub const fn new(description: &'static str) -> Self {
        Self {
            inject: Inject::new(),
            description,
            creation_url: None,
            validation: None,
            summary: None,
            setup: &[],
            docs_url: None,
        }
    }

    /// Hostnames the host injects this token into. Required; a scheme that
    /// injects nowhere can never authenticate a request.
    #[must_use]
    pub const fn inject(mut self, domains: &'static [&'static str]) -> Self {
        self.inject.domains = domains;
        self
    }

    /// Override the injection header (default `Authorization`).
    #[must_use]
    pub const fn header(mut self, header: &'static str) -> Self {
        self.inject.header = header;
        self
    }

    /// Override the value prefix (default `Bearer `). Pass `""` for a raw token.
    #[must_use]
    pub const fn prefix(mut self, prefix: &'static str) -> Self {
        self.inject.prefix = prefix;
        self
    }

    #[must_use]
    pub const fn creation_url(mut self, url: &'static str) -> Self {
        self.creation_url = Some(url);
        self
    }

    #[must_use]
    pub const fn validation(mut self, validation: Validation) -> Self {
        self.validation = Some(validation);
        self
    }

    #[must_use]
    pub const fn summary(mut self, summary: &'static str) -> Self {
        self.summary = Some(summary);
        self
    }

    #[must_use]
    pub const fn setup(mut self, steps: &'static [&'static str]) -> Self {
        self.setup = steps;
        self
    }

    #[must_use]
    pub const fn docs_url(mut self, url: &'static str) -> Self {
        self.docs_url = Some(url);
        self
    }
}

/// A host-driven OAuth scheme. `authorization_endpoint` and `token_endpoint` are
/// common to every flow; the flow-specific endpoints live in [`Flow`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OAuth {
    pub inject: Inject,
    pub display_name: &'static str,
    pub authorization_endpoint: &'static str,
    pub token_endpoint: &'static str,
    pub client_id: Option<&'static str>,
    pub scopes: &'static [&'static str],
    pub flow: Flow,
    pub summary: Option<&'static str>,
    pub setup: &'static [&'static str],
    pub docs_url: Option<&'static str>,
}

impl OAuth {
    /// Start an OAuth scheme with the device-code flow.
    #[must_use]
    pub const fn device_code(
        display_name: &'static str,
        authorization_endpoint: &'static str,
        device_authorization_endpoint: &'static str,
        token_endpoint: &'static str,
    ) -> Self {
        Self::with_flow(
            display_name,
            authorization_endpoint,
            token_endpoint,
            Flow::DeviceCode {
                device_authorization_endpoint,
            },
        )
    }

    /// Start an OAuth scheme with the PKCE loopback flow.
    #[must_use]
    pub const fn pkce_loopback(
        display_name: &'static str,
        authorization_endpoint: &'static str,
        token_endpoint: &'static str,
        redirect_uri_template: &'static str,
    ) -> Self {
        Self::with_flow(
            display_name,
            authorization_endpoint,
            token_endpoint,
            Flow::PkceLoopback {
                redirect_uri_template,
            },
        )
    }

    /// Start an OAuth scheme with the client-side-token flow.
    #[must_use]
    pub const fn client_side_token(
        display_name: &'static str,
        authorization_endpoint: &'static str,
        token_endpoint: &'static str,
        redirect_uri_template: &'static str,
    ) -> Self {
        Self::with_flow(
            display_name,
            authorization_endpoint,
            token_endpoint,
            Flow::ClientSideToken {
                redirect_uri_template,
            },
        )
    }

    const fn with_flow(
        display_name: &'static str,
        authorization_endpoint: &'static str,
        token_endpoint: &'static str,
        flow: Flow,
    ) -> Self {
        Self {
            inject: Inject::new(),
            display_name,
            authorization_endpoint,
            token_endpoint,
            client_id: None,
            scopes: &[],
            flow,
            summary: None,
            setup: &[],
            docs_url: None,
        }
    }

    /// Hostnames the host injects the obtained token into. Required.
    #[must_use]
    pub const fn inject(mut self, domains: &'static [&'static str]) -> Self {
        self.inject.domains = domains;
        self
    }

    /// Override the injection header (default `Authorization`).
    #[must_use]
    pub const fn header(mut self, header: &'static str) -> Self {
        self.inject.header = header;
        self
    }

    /// Override the value prefix (default `Bearer `).
    #[must_use]
    pub const fn prefix(mut self, prefix: &'static str) -> Self {
        self.inject.prefix = prefix;
        self
    }

    #[must_use]
    pub const fn client_id(mut self, client_id: &'static str) -> Self {
        self.client_id = Some(client_id);
        self
    }

    #[must_use]
    pub const fn scopes(mut self, scopes: &'static [&'static str]) -> Self {
        self.scopes = scopes;
        self
    }

    #[must_use]
    pub const fn summary(mut self, summary: &'static str) -> Self {
        self.summary = Some(summary);
        self
    }

    #[must_use]
    pub const fn setup(mut self, steps: &'static [&'static str]) -> Self {
        self.setup = steps;
        self
    }

    #[must_use]
    pub const fn docs_url(mut self, url: &'static str) -> Self {
        self.docs_url = Some(url);
        self
    }
}

/// The flow-specific endpoints of an OAuth scheme. The shared authorization and
/// token endpoints live on [`OAuth`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    DeviceCode {
        device_authorization_endpoint: &'static str,
    },
    PkceLoopback {
        redirect_uri_template: &'static str,
    },
    ClientSideToken {
        redirect_uri_template: &'static str,
    },
}

/// A token-validation probe `omnifs init` runs to confirm a static token works
/// and extract identity fields for the credential record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Validation {
    pub method: &'static str,
    pub url: &'static str,
    pub body: Option<&'static str>,
    pub expect_status: u16,
    pub json_pointer: Option<&'static str>,
    pub extract: &'static [Extract],
}

impl Validation {
    #[must_use]
    pub const fn get(url: &'static str) -> Self {
        Self::new("GET", url, None)
    }

    #[must_use]
    pub const fn post(url: &'static str, body: &'static str) -> Self {
        Self::new("POST", url, Some(body))
    }

    const fn new(method: &'static str, url: &'static str, body: Option<&'static str>) -> Self {
        Self {
            method,
            url,
            body,
            expect_status: 200,
            json_pointer: None,
            extract: &[],
        }
    }

    #[must_use]
    pub const fn expect_status(mut self, status: u16) -> Self {
        self.expect_status = status;
        self
    }

    #[must_use]
    pub const fn json_pointer(mut self, pointer: &'static str) -> Self {
        self.json_pointer = Some(pointer);
        self
    }

    #[must_use]
    pub const fn extract(mut self, fields: &'static [Extract]) -> Self {
        self.extract = fields;
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extract {
    pub key: &'static str,
    pub pointer: &'static str,
}

impl Extract {
    #[must_use]
    pub const fn new(key: &'static str, pointer: &'static str) -> Self {
        Self { key, pointer }
    }
}
