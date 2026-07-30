//! Host-canned explanations of the authentication mechanisms omnifs supports.
//!
//! The mechanics of each flow are identical across providers, so the prose
//! lives here (host-owned) rather than being re-authored in every provider
//! manifest. A provider manifest supplies only what is specific to it (which
//! token to create, which app to register); that guidance is paired with this
//! canned copy at the point of display by `omnifs mount add`'s auth step.

use omnifs_auth::OAuthFlow;

/// What the user actually does, a sentence or two.
pub(crate) fn experience(flow: &OAuthFlow) -> &'static str {
    match flow {
        OAuthFlow::DeviceCode(_) => {
            "omnifs shows a short code and a URL. Open the URL, enter the code, and approve. Nothing listens on a local port, so this works over SSH and on headless machines."
        },
        OAuthFlow::PkceLoopback(_) => {
            "omnifs opens your browser to the provider's consent page and listens on a localhost port. After you approve, the provider redirects back and the token is captured. Refresh tokens are supported."
        },
        OAuthFlow::PkceManualCode(_) => {
            "Like the browser-redirect flow, but for providers that don't allow a localhost redirect: after approving, copy the final redirect URL (or the `code state` pair) and paste it back here."
        },
        OAuthFlow::ClientSideToken(_) => {
            "omnifs opens your browser; the provider returns the access token directly in the redirect, with no code exchange. Used by providers that only offer this flow; usually no refresh token."
        },
    }
}

pub(crate) fn label(flow: &OAuthFlow) -> &'static str {
    match flow {
        OAuthFlow::DeviceCode(_) => "OAuth device code",
        OAuthFlow::PkceLoopback(_) => "OAuth browser redirect (PKCE)",
        OAuthFlow::PkceManualCode(_) => "OAuth paste-the-redirect (PKCE)",
        OAuthFlow::ClientSideToken(_) => "OAuth token redirect",
    }
}
