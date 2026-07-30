//! CLI-owned auth selections used while collecting credential submissions.
//!
//! Mount definitions and credential storage belong to the daemon. The CLI
//! only keeps the small set of auth options needed to drive its OAuth and
//! static-token prompts.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub(crate) enum Auth {
    StaticToken(StaticToken),
    #[serde(rename = "oauth")]
    OAuth(OAuth),
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct StaticToken {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) scheme: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) account: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct OAuth {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) scheme: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) domain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) header: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) client_secret_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) client_secret_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) redirect_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) scopes: Option<Vec<String>>,
}

impl OAuth {
    pub(crate) fn request_config(&self) -> omnifs_auth::OAuthRequestConfig {
        omnifs_auth::OAuthRequestConfig {
            scopes: self.scopes.clone(),
            domain: self.domain.clone(),
            header: self.header.clone(),
            redirect_uri: self.redirect_uri.clone(),
            client_id: self.client_id.clone(),
            client_secret_file: self.client_secret_file.clone(),
            client_secret_env: self.client_secret_env.clone(),
        }
    }
}

impl Auth {
    pub(crate) fn scheme(&self) -> Option<&str> {
        match self {
            Self::StaticToken(config) => config.scheme.as_deref(),
            Self::OAuth(config) => config.scheme.as_deref(),
        }
    }

    pub(crate) fn account(&self) -> Option<&str> {
        match self {
            Self::StaticToken(config) => config.account.as_deref(),
            Self::OAuth(config) => config.account.as_deref(),
        }
    }

    /// This auth's account label, or the fixed default every unlabeled
    /// account uses.
    pub(crate) fn account_or_default(&self) -> &str {
        self.account().unwrap_or("default")
    }

    pub(crate) fn is_oauth(&self) -> bool {
        matches!(self, Self::OAuth(_))
    }

    pub(crate) fn as_oauth(&self) -> Option<&OAuth> {
        match self {
            Self::OAuth(config) => Some(config),
            Self::StaticToken(_) => None,
        }
    }
}
