//! Host-canned explanations of the authentication mechanisms omnifs supports.
//!
//! The mechanics of each flow are identical across providers, so the prose
//! lives here (host-owned) rather than being re-authored in every provider
//! manifest. A provider manifest supplies only what is specific to it (which
//! token to create, which app to register); that guidance is paired with this
//! canned copy at the point of display by `omnifs init`'s auth step.

use crate::style;
use omnifs_provider::{OAuthFlow, SchemeGuidance};

/// An authentication mechanism omnifs knows how to drive, independent of any
/// particular provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AuthMode {
    StaticToken,
    OauthDeviceCode,
    OauthPkceLoopback,
    OauthPkceManualCode,
    OauthClientSideToken,
}

impl AuthMode {
    /// Short human label.
    pub(crate) fn label(self) -> &'static str {
        match self {
            AuthMode::StaticToken => "Static token",
            AuthMode::OauthDeviceCode => "OAuth device code",
            AuthMode::OauthPkceLoopback => "OAuth browser redirect (PKCE)",
            AuthMode::OauthPkceManualCode => "OAuth paste-the-redirect (PKCE)",
            AuthMode::OauthClientSideToken => "OAuth token redirect",
        }
    }

    /// The mode an OAuth flow drives.
    pub(crate) fn from_oauth_flow(flow: &OAuthFlow) -> AuthMode {
        match flow {
            OAuthFlow::DeviceCode(_) => AuthMode::OauthDeviceCode,
            OAuthFlow::PkceLoopback(_) => AuthMode::OauthPkceLoopback,
            OAuthFlow::PkceManualCode(_) => AuthMode::OauthPkceManualCode,
            OAuthFlow::ClientSideToken(_) => AuthMode::OauthClientSideToken,
        }
    }

    /// What the user actually does, a sentence or two.
    pub(crate) fn experience(self) -> &'static str {
        match self {
            AuthMode::StaticToken => {
                "You create a token (API key or personal access token) in the provider's web UI and paste it in. omnifs stores it and sends it on every request."
            },
            AuthMode::OauthDeviceCode => {
                "omnifs shows a short code and a URL. Open the URL, enter the code, and approve. Nothing listens on a local port, so this works over SSH and on headless machines."
            },
            AuthMode::OauthPkceLoopback => {
                "omnifs opens your browser to the provider's consent page and listens on a localhost port. After you approve, the provider redirects back and the token is captured. Refresh tokens are supported."
            },
            AuthMode::OauthPkceManualCode => {
                "Like the browser-redirect flow, but for providers that don't allow a localhost redirect: after approving, copy the final redirect URL (or the `code state` pair) and paste it back here."
            },
            AuthMode::OauthClientSideToken => {
                "omnifs opens your browser; the provider returns the access token directly in the redirect, with no code exchange. Used by providers that only offer this flow; usually no refresh token."
            },
        }
    }
}

fn print_steps_and_docs(guidance: &SchemeGuidance) {
    if !guidance.setup_steps.is_empty() {
        anstream::println!("  {}", style::dim("Setup:"));
        for (i, step) in guidance.setup_steps.iter().enumerate() {
            anstream::println!("    {}. {step}", i + 1);
        }
    }
    if let Some(url) = &guidance.docs_url {
        anstream::println!("  {} {}", style::dim("Docs:"), style::accent(url));
    }
}

/// Print what an OAuth login is about to do, plus any provider setup steps.
/// Used at login time, after the caller has printed the scheme header.
pub(crate) fn render_oauth_intro(mode: AuthMode, guidance: &SchemeGuidance) {
    anstream::println!("  {}", style::dim(mode.experience()));
    print_steps_and_docs(guidance);
}

/// Print how to obtain a static token before prompting the user for one.
pub(crate) fn render_static_token_intro(creation_url: Option<&str>, guidance: &SchemeGuidance) {
    anstream::println!("  {}", style::dim(AuthMode::StaticToken.experience()));
    if let Some(url) = creation_url {
        anstream::println!(
            "  {} {}",
            style::dim("Create a token at:"),
            style::accent(url)
        );
    }
    print_steps_and_docs(guidance);
}
