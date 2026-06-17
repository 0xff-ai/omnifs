//! Host-canned explanations of the authentication mechanisms omnifs supports.
//!
//! The mechanics of each flow are identical across providers, so the prose
//! lives here (host-owned) rather than being re-authored in every provider
//! manifest. A provider manifest supplies only what is specific to it (which
//! token to create, which app to register); that guidance is paired with this
//! canned copy at the point of display.

use crate::style;
use omnifs_provider::{AuthScheme, OAuthFlow, ProviderAuthManifest, SchemeGuidance};

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
    /// Every mode, in rough order of how commonly a user meets it.
    pub(crate) const ALL: [AuthMode; 5] = [
        AuthMode::StaticToken,
        AuthMode::OauthPkceLoopback,
        AuthMode::OauthDeviceCode,
        AuthMode::OauthPkceManualCode,
        AuthMode::OauthClientSideToken,
    ];

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

    /// One-line summary for pickers and listings.
    pub(crate) fn summary(self) -> &'static str {
        match self {
            AuthMode::StaticToken => "Paste a long-lived token you create in the provider's settings.",
            AuthMode::OauthDeviceCode => "Approve a short code in your browser; works on headless hosts.",
            AuthMode::OauthPkceLoopback => "Your browser opens, you approve, and you're redirected back automatically.",
            AuthMode::OauthPkceManualCode => "Your browser opens; you paste the final redirect URL back here.",
            AuthMode::OauthClientSideToken => "Your browser opens; the access token comes back in the redirect.",
        }
    }

    /// The mode a concrete scheme drives, or `None` for [`AuthScheme::None`].
    pub(crate) fn from_scheme(scheme: &AuthScheme) -> Option<AuthMode> {
        match scheme {
            AuthScheme::None => None,
            AuthScheme::StaticToken(_) => Some(AuthMode::StaticToken),
            AuthScheme::Oauth(oauth) => Some(AuthMode::from_oauth_flow(&oauth.flow)),
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

/// Render the general catalog of supported auth mechanisms for `omnifs auth modes`.
pub(crate) fn render_modes_catalog() {
    anstream::println!("{}", style::bold("Authentication modes omnifs supports"));
    anstream::println!(
        "{}",
        style::dim("How a provider authenticates is declared in its manifest; run `omnifs auth explain <provider>` for a specific one.")
    );
    for mode in AuthMode::ALL {
        anstream::println!();
        anstream::println!("{}", style::accent(mode.label()));
        anstream::println!("  {}", mode.summary());
        anstream::println!("  {}", style::dim(mode.experience()));
    }
    anstream::println!();
    anstream::println!(
        "{}",
        style::dim("Some providers need no auth at all (public APIs). OAuth flows may use omnifs's registered app or your own: when a provider ships no client id, you create an app and supply its client id and secret.")
    );
}

/// Render every auth scheme a provider declares, pairing the host's canned
/// flow-kind copy with the provider's own setup guidance.
pub(crate) fn render_provider_auth(provider_label: &str, auth: &ProviderAuthManifest) {
    anstream::println!(
        "{}",
        style::bold(format!("Authentication for {provider_label}"))
    );
    anstream::println!(
        "  {}",
        style::dim(format!("Applies to: {}", auth.inject.domains.join(", ")))
    );
    for (key, scheme) in &auth.schemes {
        anstream::println!();
        render_scheme(key, scheme, &auth.guidance_for(key), *key == auth.default);
    }
}

fn render_scheme(key: &str, scheme: &AuthScheme, guidance: &SchemeGuidance, is_default: bool) {
    let mode = AuthMode::from_scheme(scheme);
    let mode_label = mode.map_or("unknown", AuthMode::label);
    let default_tag = if is_default {
        format!(" {}", style::dim("(default)"))
    } else {
        String::new()
    };
    anstream::println!(
        "{} {}{}",
        style::accent(key),
        style::dim(format!("— {mode_label}")),
        default_tag
    );

    // Provider one-liner if it gave one, else the canned mode summary.
    match (&guidance.summary, mode) {
        (Some(summary), _) => anstream::println!("  {summary}"),
        (None, Some(mode)) => anstream::println!("  {}", mode.summary()),
        (None, None) => {},
    }
    if let Some(mode) = mode {
        anstream::println!("  {}", style::dim(mode.experience()));
    }

    match scheme {
        AuthScheme::StaticToken(s) => {
            if let Some(url) = &s.creation_url {
                anstream::println!(
                    "  {} {}",
                    style::dim("Create a token at:"),
                    style::accent(url)
                );
            }
        },
        AuthScheme::Oauth(o) => {
            if !o.default_scopes.is_empty() {
                anstream::println!("  {} {}", style::dim("Scopes:"), o.default_scopes.join(", "));
            }
            if o.default_client_id.is_none() {
                anstream::println!(
                    "  {}",
                    style::warn("Needs your own OAuth app: omnifs ships no client id for this scheme.")
                );
            }
        },
        AuthScheme::None => {},
    }

    print_steps_and_docs(guidance);
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
