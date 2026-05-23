use crate::client::AuthError;
use oauth2::basic::BasicClient;
use oauth2::{
    AuthType, AuthUrl, ClientId, ClientSecret, DeviceAuthorizationUrl, EndpointMaybeSet,
    EndpointNotSet, EndpointSet, RedirectUrl, RevocationUrl, TokenUrl,
};
use omnifs_mount_schema::{
    DeviceCodeConfig, OAuthFlow, OauthScheme, PkceLoopbackConfig, PkceManualCodeConfig,
    TokenEndpointAuthMethod,
};
use secrecy::{ExposeSecret, SecretString};

pub(crate) type ConfiguredClient =
    BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointMaybeSet, EndpointSet>;
pub(crate) type ConfiguredDeviceClient =
    BasicClient<EndpointSet, EndpointSet, EndpointNotSet, EndpointMaybeSet, EndpointSet>;

#[derive(Clone, Debug)]
pub struct OAuthRequest {
    pub(crate) scheme: OauthScheme,
    client_id: Option<String>,
    client_secret: Option<SecretString>,
}

impl OAuthRequest {
    pub fn new(scheme: OauthScheme) -> Self {
        Self {
            scheme,
            client_id: None,
            client_secret: None,
        }
    }

    #[must_use]
    pub fn from_config(mut scheme: OauthScheme, config: OAuthRequestConfig) -> Self {
        if let Some(scopes) = config.scopes {
            scheme.default_scopes = scopes;
        }
        if let Some(domain) = config.inject_domain {
            scheme.inject_domains = vec![domain];
        }
        if let Some(header) = config.inject_header {
            scheme.inject_header_name = Some(header);
        }
        if let Some(redirect_uri) = config.redirect_uri {
            override_redirect_uri(&mut scheme, redirect_uri);
        }

        let mut request = Self::new(scheme);
        if let Some(client_id) = config.client_id {
            request = request.with_client_id(client_id);
        }
        if let Some(client_secret) = config.client_secret {
            request = request.with_client_secret(client_secret);
        }
        request
    }

    #[must_use]
    pub fn with_client_id(mut self, client_id: impl Into<String>) -> Self {
        self.client_id = Some(client_id.into());
        self
    }

    #[must_use]
    pub fn with_client_secret(mut self, client_secret: SecretString) -> Self {
        self.client_secret = Some(client_secret);
        self
    }

    #[must_use]
    pub fn scheme(&self) -> &OauthScheme {
        &self.scheme
    }

    #[must_use]
    pub fn client_id(&self) -> Option<&str> {
        self.client_id.as_deref()
    }

    #[must_use]
    pub fn supports_revocation(&self) -> bool {
        self.scheme.revocation_endpoint.is_some()
    }

    pub fn override_default_scopes(&mut self, scopes: Vec<String>) {
        self.scheme.default_scopes = scopes;
    }

    pub(crate) fn client(&self, redirect_uri: RedirectUrl) -> Result<ConfiguredClient, AuthError> {
        Ok(self.token_client()?.set_redirect_uri(redirect_uri))
    }

    pub(crate) fn token_client(&self) -> Result<ConfiguredClient, AuthError> {
        let auth_uri = AuthUrl::new(self.scheme.authorization_endpoint.clone())?;
        let token_uri = TokenUrl::new(self.scheme.token_endpoint.clone())?;
        let revocation_url = self
            .scheme
            .revocation_endpoint
            .clone()
            .map(RevocationUrl::new)
            .transpose()?;

        let mut client = BasicClient::new(ClientId::new(self.effective_client_id()?))
            .set_auth_uri(auth_uri)
            .set_token_uri(token_uri)
            .set_revocation_url_option(revocation_url);

        if let Some((secret, auth_type)) = token_endpoint_secret(
            &self.scheme.token_endpoint_auth,
            self.client_secret.as_ref(),
        )? {
            client = client.set_client_secret(secret);
            client = client.set_auth_type(auth_type);
        }
        Ok(client)
    }

    pub(crate) fn device_client(
        &self,
        config: &DeviceCodeConfig,
    ) -> Result<ConfiguredDeviceClient, AuthError> {
        let auth_uri = AuthUrl::new(self.scheme.authorization_endpoint.clone())?;
        let token_uri = TokenUrl::new(self.scheme.token_endpoint.clone())?;
        let device_uri = DeviceAuthorizationUrl::new(config.device_authorization_endpoint.clone())?;
        let revocation_url = self
            .scheme
            .revocation_endpoint
            .clone()
            .map(RevocationUrl::new)
            .transpose()?;

        let mut client = BasicClient::new(ClientId::new(self.effective_client_id()?))
            .set_auth_uri(auth_uri)
            .set_token_uri(token_uri)
            .set_device_authorization_url(device_uri)
            .set_revocation_url_option(revocation_url);

        if let Some((secret, auth_type)) = token_endpoint_secret(
            &self.scheme.token_endpoint_auth,
            self.client_secret.as_ref(),
        )? {
            client = client.set_client_secret(secret);
            client = client.set_auth_type(auth_type);
        }
        Ok(client)
    }

    #[must_use]
    pub fn into_login_request(self) -> LoginRequest {
        match &self.scheme.flow {
            OAuthFlow::PkceLoopback(flow) => LoginRequest::Loopback(LoopbackLoginRequest {
                flow: flow.clone(),
                oauth: self,
            }),
            OAuthFlow::PkceManualCode(flow) => LoginRequest::ManualCode(ManualCodeLoginRequest {
                flow: flow.clone(),
                oauth: self,
            }),
            OAuthFlow::DeviceCode(flow) => LoginRequest::DeviceCode(DeviceCodeLoginRequest {
                flow: flow.clone(),
                oauth: self,
            }),
        }
    }

    fn effective_client_id(&self) -> Result<String, AuthError> {
        self.client_id
            .clone()
            .or_else(|| self.scheme.default_client_id.clone())
            .ok_or(AuthError::MissingClientId)
    }
}

#[derive(Default)]
pub struct OAuthRequestConfig {
    scopes: Option<Vec<String>>,
    inject_domain: Option<String>,
    inject_header: Option<String>,
    redirect_uri: Option<String>,
    client_id: Option<String>,
    client_secret: Option<SecretString>,
}

impl OAuthRequestConfig {
    #[must_use]
    pub fn with_scopes(mut self, scopes: Vec<String>) -> Self {
        self.scopes = Some(scopes);
        self
    }

    #[must_use]
    pub fn with_inject_domain(mut self, domain: impl Into<String>) -> Self {
        self.inject_domain = Some(domain.into());
        self
    }

    #[must_use]
    pub fn with_inject_header(mut self, header: impl Into<String>) -> Self {
        self.inject_header = Some(header.into());
        self
    }

    #[must_use]
    pub fn with_redirect_uri(mut self, redirect_uri: impl Into<String>) -> Self {
        self.redirect_uri = Some(redirect_uri.into());
        self
    }

    #[must_use]
    pub fn with_client_id(mut self, client_id: impl Into<String>) -> Self {
        self.client_id = Some(client_id.into());
        self
    }

    #[must_use]
    pub fn with_client_secret(mut self, client_secret: SecretString) -> Self {
        self.client_secret = Some(client_secret);
        self
    }
}

fn override_redirect_uri(scheme: &mut OauthScheme, redirect_uri: String) {
    match &mut scheme.flow {
        OAuthFlow::PkceLoopback(flow) => {
            flow.redirect_uri_template = redirect_uri;
        },
        OAuthFlow::PkceManualCode(flow) => {
            flow.redirect_uri = redirect_uri;
        },
        OAuthFlow::DeviceCode(_) => {},
    }
}

#[derive(Clone, Debug)]
pub enum LoginRequest {
    Loopback(LoopbackLoginRequest),
    ManualCode(ManualCodeLoginRequest),
    DeviceCode(DeviceCodeLoginRequest),
}

#[derive(Clone, Debug)]
pub struct LoopbackLoginRequest {
    pub(crate) oauth: OAuthRequest,
    pub(crate) flow: PkceLoopbackConfig,
}

#[derive(Clone, Debug)]
pub struct ManualCodeLoginRequest {
    pub(crate) oauth: OAuthRequest,
    pub(crate) flow: PkceManualCodeConfig,
}

#[derive(Clone, Debug)]
pub struct DeviceCodeLoginRequest {
    pub(crate) oauth: OAuthRequest,
    pub(crate) flow: DeviceCodeConfig,
}

fn token_endpoint_secret(
    method: &TokenEndpointAuthMethod,
    secret: Option<&SecretString>,
) -> Result<Option<(ClientSecret, AuthType)>, AuthError> {
    let auth_type = match method {
        TokenEndpointAuthMethod::None => return Ok(None),
        TokenEndpointAuthMethod::ClientSecretPost => AuthType::RequestBody,
        TokenEndpointAuthMethod::ClientSecretBasic => AuthType::BasicAuth,
    };
    let secret = secret.ok_or(AuthError::MissingClientSecret)?;
    Ok(Some((
        ClientSecret::new(secret.expose_secret().to_owned()),
        auth_type,
    )))
}
