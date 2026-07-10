use crate::error::AuthError;
use oauth2::basic::BasicClient;
use oauth2::{
    AuthType, AuthUrl, ClientId, ClientSecret, DeviceAuthorizationUrl, EndpointMaybeSet,
    EndpointNotSet, EndpointSet, RedirectUrl, RevocationUrl, TokenUrl,
};
use omnifs_workspace::authn::{
    ClientSideTokenConfig, DeviceCodeConfig, OAuthFlow, OauthScheme, PkceLoopbackConfig,
    PkceManualCodeConfig, TokenEndpointAuthMethod,
};
use omnifs_workspace::mounts::OAuth;
use secrecy::{ExposeSecret, SecretString};

pub(crate) type ConfiguredClient =
    BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointMaybeSet, EndpointSet>;
pub(crate) type ConfiguredAuthorizationClient =
    BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointNotSet>;
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
    pub fn scheme(&self) -> &OauthScheme {
        &self.scheme
    }

    pub fn override_default_scopes(&mut self, scopes: Vec<String>) {
        self.scheme.default_scopes = scopes;
    }

    pub(crate) fn client(&self, redirect_uri: RedirectUrl) -> Result<ConfiguredClient, AuthError> {
        Ok(self.token_client()?.set_redirect_uri(redirect_uri))
    }

    pub(crate) fn authorization_client(
        &self,
        redirect_uri: RedirectUrl,
    ) -> Result<ConfiguredAuthorizationClient, AuthError> {
        let auth_uri = AuthUrl::new(self.scheme.authorization_endpoint.clone())?;
        Ok(BasicClient::new(ClientId::new(self.effective_client_id()?))
            .set_auth_uri(auth_uri)
            .set_redirect_uri(redirect_uri))
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

        if let Some((secret, auth_type)) = self.token_endpoint_secret()? {
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

        if let Some((secret, auth_type)) = self.token_endpoint_secret()? {
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
            OAuthFlow::ClientSideToken(flow) => {
                LoginRequest::ClientSideToken(ClientSideTokenLoginRequest {
                    flow: flow.clone(),
                    oauth: self,
                })
            },
            OAuthFlow::DeviceCode(flow) => LoginRequest::DeviceCode(DeviceCodeLoginRequest {
                flow: flow.clone(),
                oauth: self,
            }),
        }
    }

    pub(crate) fn effective_client_id(&self) -> Result<String, AuthError> {
        self.client_id
            .clone()
            .or_else(|| self.scheme.default_client_id.clone())
            .ok_or(AuthError::MissingClientId)
    }

    fn token_endpoint_secret(&self) -> Result<Option<(ClientSecret, AuthType)>, AuthError> {
        let auth_type = match self.scheme.token_endpoint_auth {
            TokenEndpointAuthMethod::None => return Ok(None),
            TokenEndpointAuthMethod::ClientSecretPost => AuthType::RequestBody,
            TokenEndpointAuthMethod::ClientSecretBasic => AuthType::BasicAuth,
        };
        let secret = self
            .client_secret
            .as_ref()
            .ok_or(AuthError::MissingClientSecret)?;
        Ok(Some((
            ClientSecret::new(secret.expose_secret().to_owned()),
            auth_type,
        )))
    }

    /// Build an [`OAuthRequest`] from a mount's `auth` config block, applying
    /// scope, injection, redirect, and client credential overrides.
    pub fn from_mount_config(
        config: Option<&OAuth>,
        mut scheme: OauthScheme,
    ) -> Result<OAuthRequest, AuthError> {
        let Some(config) = config else {
            return Ok(OAuthRequest::new(scheme));
        };

        if let Some(scopes) = &config.scopes {
            scheme.default_scopes.clone_from(scopes);
        }
        if let Some(domain) = non_empty_config_value(config.domain.as_deref(), "auth.domain")? {
            scheme.inject_domains = vec![domain.to_owned()];
        }
        if let Some(header) = non_empty_config_value(config.header.as_deref(), "auth.header")? {
            scheme.inject_header_name = Some(header.to_owned());
        }
        if let Some(redirect_uri) =
            non_empty_config_value(config.redirect_uri.as_deref(), "auth.redirectUri")?
        {
            override_redirect_uri(&mut scheme, redirect_uri.to_owned());
        }

        let mut request = OAuthRequest::new(scheme);
        if let Some(client_id) =
            non_empty_config_value(config.client_id.as_deref(), "auth.clientId")?
        {
            request.client_id = Some(client_id.to_owned());
        }
        if let Some(client_secret) = read_oauth_client_secret(config)? {
            request.client_secret = Some(client_secret);
        }
        Ok(request)
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
        OAuthFlow::ClientSideToken(flow) => {
            flow.redirect_uri_template = redirect_uri;
        },
        OAuthFlow::DeviceCode(_) => {},
    }
}

#[derive(Clone, Debug)]
pub enum LoginRequest {
    Loopback(LoopbackLoginRequest),
    ManualCode(ManualCodeLoginRequest),
    ClientSideToken(ClientSideTokenLoginRequest),
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
pub struct ClientSideTokenLoginRequest {
    pub(crate) oauth: OAuthRequest,
    pub(crate) flow: ClientSideTokenConfig,
}

#[derive(Clone, Debug)]
pub struct DeviceCodeLoginRequest {
    pub(crate) oauth: OAuthRequest,
    pub(crate) flow: DeviceCodeConfig,
}

fn read_oauth_client_secret(config: &OAuth) -> Result<Option<SecretString>, AuthError> {
    if let Some(path) = config.client_secret_file.as_deref() {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                return secret_from_config_value(
                    contents.trim(),
                    &format!("auth.clientSecretFile {path}"),
                );
            },
            Err(error) if config.client_secret_env.is_none() => {
                return Err(AuthError::RequestConfig(format!(
                    "failed to read auth.clientSecretFile {path}: {error}"
                )));
            },
            Err(_) => {},
        }
    }

    let Some(env_var) = config.client_secret_env.as_deref() else {
        return Ok(None);
    };
    let value = std::env::var(env_var).map_err(|error| {
        AuthError::RequestConfig(format!(
            "failed to read auth.clientSecretEnv {env_var}: {error}"
        ))
    })?;
    secret_from_config_value(value.trim(), &format!("auth.clientSecretEnv {env_var}"))
}

fn secret_from_config_value(value: &str, source: &str) -> Result<Option<SecretString>, AuthError> {
    non_empty_config_value(Some(value), source)
        .map(|value| value.map(|value| SecretString::from(value.to_owned())))
}

fn non_empty_config_value<'a>(
    value: Option<&'a str>,
    source: &str,
) -> Result<Option<&'a str>, AuthError> {
    let Some(value) = value.map(str::trim) else {
        return Ok(None);
    };
    if value.is_empty() {
        return Err(AuthError::RequestConfig(format!("{source} is empty")));
    }
    Ok(Some(value))
}
