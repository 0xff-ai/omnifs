//! Host-canned explanations of the authentication mechanisms omnifs supports.
//!
//! The mechanics of each flow are identical across providers, so the prose
//! lives here (host-owned) rather than being re-authored in every provider
//! manifest. A provider manifest supplies only what is specific to it (which
//! token to create, which app to register); that guidance is paired with this
//! canned copy at the point of display by `omnifs mount add`'s auth step.

use std::fmt;

use omnifs_workspace::authn::OAuthFlow;

/// An authentication mechanism omnifs knows how to drive, independent of any
/// particular provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AuthMode {
    DeviceCode,
    PkceLoopback,
    PkceManualCode,
    ClientSideToken,
}

impl AuthMode {
    /// What the user actually does, a sentence or two.
    pub(crate) fn experience(self) -> &'static str {
        match self {
            AuthMode::DeviceCode => {
                "omnifs shows a short code and a URL. Open the URL, enter the code, and approve. Nothing listens on a local port, so this works over SSH and on headless machines."
            },
            AuthMode::PkceLoopback => {
                "omnifs opens your browser to the provider's consent page and listens on a localhost port. After you approve, the provider redirects back and the token is captured. Refresh tokens are supported."
            },
            AuthMode::PkceManualCode => {
                "Like the browser-redirect flow, but for providers that don't allow a localhost redirect: after approving, copy the final redirect URL (or the `code state` pair) and paste it back here."
            },
            AuthMode::ClientSideToken => {
                "omnifs opens your browser; the provider returns the access token directly in the redirect, with no code exchange. Used by providers that only offer this flow; usually no refresh token."
            },
        }
    }
}

impl From<&OAuthFlow> for AuthMode {
    fn from(flow: &OAuthFlow) -> Self {
        match flow {
            OAuthFlow::DeviceCode(_) => Self::DeviceCode,
            OAuthFlow::PkceLoopback(_) => Self::PkceLoopback,
            OAuthFlow::PkceManualCode(_) => Self::PkceManualCode,
            OAuthFlow::ClientSideToken(_) => Self::ClientSideToken,
        }
    }
}

impl fmt::Display for AuthMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::DeviceCode => "OAuth device code",
            Self::PkceLoopback => "OAuth browser redirect (PKCE)",
            Self::PkceManualCode => "OAuth paste-the-redirect (PKCE)",
            Self::ClientSideToken => "OAuth token redirect",
        })
    }
}
