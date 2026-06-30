use crate::{AuthManifest, AuthScheme, OauthScheme, StaticTokenScheme};
use thiserror::Error;

const STATIC_KIND: &str = "static-token";
const OAUTH_KIND: &str = "oauth";

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SchemeResolveError {
    #[error("no {kind} auth scheme `{key}`")]
    NotFound { kind: &'static str, key: String },
    #[error("multiple {kind} auth schemes declared; set auth.scheme")]
    Ambiguous { kind: &'static str },
    #[error("auth manifest declares no {kind} auth scheme")]
    NoSchemes { kind: &'static str },
}

impl AuthManifest {
    pub fn resolve_static_scheme(
        &self,
        key: Option<&str>,
    ) -> Result<&StaticTokenScheme, SchemeResolveError> {
        let mut schemes = self.schemes.iter().filter_map(|scheme| match scheme {
            AuthScheme::StaticToken(static_token) => Some(static_token),
            AuthScheme::None | AuthScheme::Oauth(_) => None,
        });
        if let Some(key) = key {
            return schemes.find(|scheme| scheme.key == key).ok_or_else(|| {
                SchemeResolveError::NotFound {
                    kind: STATIC_KIND,
                    key: key.to_owned(),
                }
            });
        }
        let Some(first) = schemes.next() else {
            return Err(SchemeResolveError::NoSchemes { kind: STATIC_KIND });
        };
        if schemes.next().is_some() {
            return Err(SchemeResolveError::Ambiguous { kind: STATIC_KIND });
        }
        Ok(first)
    }

    pub fn resolve_oauth_scheme(
        &self,
        key: Option<&str>,
    ) -> Result<&OauthScheme, SchemeResolveError> {
        let mut schemes = self.schemes.iter().filter_map(|scheme| match scheme {
            AuthScheme::Oauth(oauth) => Some(oauth),
            AuthScheme::None | AuthScheme::StaticToken(_) => None,
        });
        if let Some(key) = key {
            return schemes.find(|scheme| scheme.key == key).ok_or_else(|| {
                SchemeResolveError::NotFound {
                    kind: OAUTH_KIND,
                    key: key.to_owned(),
                }
            });
        }
        let Some(first) = schemes.next() else {
            return Err(SchemeResolveError::NoSchemes { kind: OAUTH_KIND });
        };
        if schemes.next().is_some() {
            return Err(SchemeResolveError::Ambiguous { kind: OAUTH_KIND });
        }
        Ok(first)
    }

    #[must_use]
    pub fn first_static_scheme_key(&self) -> Option<&str> {
        self.schemes.iter().find_map(|scheme| match scheme {
            AuthScheme::StaticToken(static_token) => Some(static_token.key.as_str()),
            AuthScheme::None | AuthScheme::Oauth(_) => None,
        })
    }

    #[must_use]
    pub fn static_scheme_count(&self) -> usize {
        self.schemes
            .iter()
            .filter(|scheme| matches!(scheme, AuthScheme::StaticToken(_)))
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{OAuthFlow, PkceManualCodeConfig, TokenEndpointAuthMethod};

    fn static_scheme(key: &str) -> AuthScheme {
        AuthScheme::StaticToken(StaticTokenScheme {
            key: key.to_string(),
            header_name: None,
            value_prefix: "Bearer ".to_string(),
            description: key.to_string(),
            inject_domains: vec!["api.example.com".to_string()],
            creation_url: None,
            validation: None,
        })
    }

    fn oauth_scheme(key: &str) -> AuthScheme {
        AuthScheme::Oauth(OauthScheme {
            key: key.to_string(),
            display_name: key.to_string(),
            authorization_endpoint: "http://localhost/authorize".to_string(),
            token_endpoint: "http://localhost/token".to_string(),
            revocation_endpoint: None,
            default_client_id: None,
            default_scopes: vec![],
            flow: OAuthFlow::PkceManualCode(PkceManualCodeConfig {
                redirect_uri: "http://localhost/callback".to_string(),
            }),
            token_endpoint_auth: TokenEndpointAuthMethod::None,
            refresh_token_rotates: false,
            extra_authorize_params: vec![],
            extra_token_params: vec![],
            inject_domains: vec!["api.example.com".to_string()],
            inject_header_name: None,
            inject_value_prefix: "Bearer ".to_string(),
        })
    }

    #[test]
    fn resolve_static_scheme_key_selection() {
        let multi = AuthManifest {
            schemes: vec![static_scheme("pat"), static_scheme("api-key")],
        };
        assert_eq!(
            multi.resolve_static_scheme(Some("api-key")).unwrap().key,
            "api-key"
        );
        assert_eq!(
            multi.resolve_static_scheme(None),
            Err(SchemeResolveError::Ambiguous { kind: STATIC_KIND })
        );
        assert_eq!(
            multi.resolve_static_scheme(Some("missing")),
            Err(SchemeResolveError::NotFound {
                kind: STATIC_KIND,
                key: "missing".to_string(),
            })
        );

        let single = AuthManifest {
            schemes: vec![static_scheme("pat")],
        };
        assert_eq!(single.resolve_static_scheme(None).unwrap().key, "pat");
    }

    #[test]
    fn resolve_oauth_scheme_key_selection() {
        let multi = AuthManifest {
            schemes: vec![oauth_scheme("oauth"), oauth_scheme("enterprise")],
        };
        assert_eq!(
            multi.resolve_oauth_scheme(Some("enterprise")).unwrap().key,
            "enterprise"
        );
        assert_eq!(
            multi.resolve_oauth_scheme(None),
            Err(SchemeResolveError::Ambiguous { kind: OAUTH_KIND })
        );
        assert_eq!(
            multi.resolve_oauth_scheme(Some("missing")),
            Err(SchemeResolveError::NotFound {
                kind: OAUTH_KIND,
                key: "missing".to_string(),
            })
        );

        let single = AuthManifest {
            schemes: vec![oauth_scheme("oauth")],
        };
        assert_eq!(single.resolve_oauth_scheme(None).unwrap().key, "oauth");
    }

    #[test]
    fn resolve_static_scheme_reports_no_schemes() {
        let manifest = AuthManifest { schemes: vec![] };
        assert_eq!(
            manifest.resolve_static_scheme(None),
            Err(SchemeResolveError::NoSchemes { kind: STATIC_KIND })
        );
    }

    #[test]
    fn resolve_oauth_scheme_reports_no_schemes() {
        let manifest = AuthManifest {
            schemes: vec![static_scheme("pat")],
        };
        assert_eq!(
            manifest.resolve_oauth_scheme(None),
            Err(SchemeResolveError::NoSchemes { kind: OAUTH_KIND })
        );
    }
}
