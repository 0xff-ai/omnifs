//! Guest-side provider auth metadata.
//!
//! These types are intentionally static-slice shaped: the provider macro embeds
//! auth in the provider metadata const, and rustc evaluates that const into the
//! Wasm metadata section.

/// A provider's auth manifest: how the host injects credentials, which scheme
/// `omnifs init` defaults to, and the schemes a user can pick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Auth {
    pub inject: Inject,
    pub default: &'static str,
    pub schemes: &'static [SchemeEntry],
}

impl Auth {
    /// Start an auth manifest. `domains` are the hostnames the host injects the
    /// credential into; `default` names the scheme `omnifs init` picks when the
    /// user makes no explicit choice.
    pub const fn new(
        domains: &'static [&'static str],
        default: &'static str,
        schemes: &'static [SchemeEntry],
    ) -> Self {
        Self {
            inject: Inject {
                domains,
                header: "Authorization",
                prefix: "Bearer ",
            },
            default,
            schemes,
        }
    }

    #[must_use]
    pub const fn header(mut self, header: &'static str) -> Self {
        self.inject.header = header;
        self
    }

    #[must_use]
    pub const fn prefix(mut self, prefix: &'static str) -> Self {
        self.inject.prefix = prefix;
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inject {
    pub domains: &'static [&'static str],
    pub header: &'static str,
    pub prefix: &'static str,
}

/// One keyed auth scheme.
pub type SchemeEntry = (&'static str, Scheme);

/// One auth scheme: a user-supplied static token or a host-driven OAuth flow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    StaticToken(StaticToken),
    Oauth(OAuth),
}

/// A bring-your-own static token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StaticToken {
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
            description,
            creation_url: None,
            validation: None,
            summary: None,
            setup: &[],
            docs_url: None,
        }
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

/// A host-driven OAuth scheme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OAuth {
    pub display_name: &'static str,
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
            Flow::DeviceCode {
                authorization_endpoint,
                device_authorization_endpoint,
                token_endpoint,
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
            Flow::PkceLoopback {
                authorization_endpoint,
                token_endpoint,
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
            Flow::ClientSideToken {
                authorization_endpoint,
                token_endpoint,
                redirect_uri_template,
            },
        )
    }

    const fn with_flow(display_name: &'static str, flow: Flow) -> Self {
        Self {
            display_name,
            client_id: None,
            scopes: &[],
            flow,
            summary: None,
            setup: &[],
            docs_url: None,
        }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    DeviceCode {
        authorization_endpoint: &'static str,
        device_authorization_endpoint: &'static str,
        token_endpoint: &'static str,
    },
    PkceLoopback {
        authorization_endpoint: &'static str,
        token_endpoint: &'static str,
        redirect_uri_template: &'static str,
    },
    ClientSideToken {
        authorization_endpoint: &'static str,
        token_endpoint: &'static str,
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
