//! OAuth login flow.

use crate::error::{ExitCode, WithExitCode, WithHint};
use anyhow::anyhow;
use omnifs_api::{
    CredentialClientOverrides, CredentialMaterial, CredentialSubmission, SecretBytes,
};
use omnifs_auth::CredentialEntry;
use omnifs_auth::{
    DeviceCodePrompt, LoginRequest, ManualCode, ManualCodeLoginRequest, OAuthClient, OAuthRequest,
    UrlOpener,
};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use super::Auth;
use crate::ui::style;
use omnifs_auth::SchemeGuidance;
use omnifs_core::ProviderId;
use omnifs_provider::ProviderManifest;
use secrecy::ExposeSecret;

const MANUAL_PROMPT_CANCELED: &str = "omnifs-manual-oauth-prompt-canceled";

/// Whether to suppress the system browser and whether prompts are allowed.
/// Bundled so the credential-submission flow keeps a readable argument list
/// instead of carrying three separate positional bools/slices.
#[derive(Clone, Copy)]
pub(crate) struct LoginInteractivity<'a> {
    pub(crate) no_browser: bool,
    pub(crate) no_input: bool,
    /// `None` uses the provider or auth-config default. `Some([])` is an
    /// explicit empty scope set and must not fall back to provider defaults.
    pub(crate) scopes: Option<&'a [String]>,
}

/// Run the provider-declared OAuth flow for a daemon credential submission.
/// This performs only the user-facing flow. The daemon receives the resulting
/// material and owns all durable credential state and upstream revocation.
pub(crate) async fn login_for_submission(
    provider: ProviderId,
    manifest: &ProviderManifest,
    auth: &Auth,
    account_label: &str,
    interactivity: LoginInteractivity<'_>,
    output: &crate::ui::output::Output,
    key_width: usize,
) -> anyhow::Result<CredentialSubmission> {
    let scheme_key = auth
        .scheme()
        .ok_or_else(|| anyhow!("auth config must set a scheme"))?;
    let auth_manifest = manifest
        .auth
        .as_ref()
        .ok_or_else(|| anyhow!("provider `{}` has no auth manifest", manifest.id))?
        .wasm_auth_manifest();
    let scheme = auth_manifest
        .resolve_oauth_scheme(Some(scheme_key))?
        .clone();
    let config = auth.as_oauth().map(super::OAuth::request_config);
    let mut request = OAuthRequest::from_config(config.as_ref(), scheme)?;
    if let Some(scopes) = interactivity.scopes {
        request.override_default_scopes(scopes.to_vec());
    }
    let guidance = manifest
        .auth
        .as_ref()
        .map(|auth| auth.guidance_for(scheme_key))
        .unwrap_or_default();
    let entry = run_oauth(
        request,
        manifest.id.as_str(),
        &guidance,
        interactivity,
        output,
        key_width,
    )
    .await?;
    let requested_scopes = interactivity
        .scopes
        .map(<[String]>::to_vec)
        .or_else(|| auth.as_oauth().and_then(|oauth| oauth.scopes.clone()));
    let expires_at_unix = entry.expires_at().map(time::OffsetDateTime::unix_timestamp);
    let refresh_token = entry
        .refresh_token()
        .map(|value| SecretBytes::new(value.expose_secret().as_bytes().to_vec()));
    Ok(CredentialSubmission {
        provider,
        scheme: scheme_key.to_owned(),
        account_label: account_label.to_owned(),
        material: CredentialMaterial::OAuth {
            access_token: SecretBytes::new(
                entry.access_token().expose_secret().as_bytes().to_vec(),
            ),
            refresh_token,
            expires_at_unix,
            token_type: entry.token_type().to_owned(),
            scopes: entry.scopes().to_vec(),
            upstream_identity: entry.upstream_identity().map(str::to_owned),
        },
        overrides: CredentialClientOverrides {
            client_id: auth.as_oauth().and_then(|oauth| oauth.client_id.clone()),
            client_secret: None,
            redirect_uri: auth.as_oauth().and_then(|oauth| oauth.redirect_uri.clone()),
            scopes: requested_scopes,
        },
    })
}

async fn run_oauth(
    request: OAuthRequest,
    mount: &str,
    guidance: &SchemeGuidance,
    interactivity: LoginInteractivity<'_>,
    output: &crate::ui::output::Output,
    key_width: usize,
) -> anyhow::Result<CredentialEntry> {
    let LoginInteractivity {
        no_browser,
        no_input,
        ..
    } = interactivity;
    output.narrate(format!(
        "requesting OAuth for `{mount}` using scheme `{}` ({})",
        request.scheme().key,
        super::explain::label(&request.scheme().flow)
    ));
    print_oauth_consent_summary(output, &request, guidance);
    let client = OAuthClient::new()?;
    // The opener runs inside `OAuthClient`'s own flow, potentially from a
    // different call frame than this one, so it needs its own owned handle
    // rather than a borrow.
    let client = if no_browser {
        client.with_opener(Arc::new(PrintOpener {
            output: output.clone(),
        }))
    } else {
        client.with_system_browser()
    };
    let entry = match request.into_login_request() {
        LoginRequest::Loopback(request) => client
            .login_loopback(request)
            .await
            .with_hint(format!("Re-run `omnifs mount reauth {mount}` to retry"))?,
        LoginRequest::ClientSideToken(request) => client
            .login_client_side_token(request)
            .await
            .with_hint(format!("Re-run `omnifs mount reauth {mount}` to retry"))?,
        LoginRequest::ManualCode(_) if no_input => {
            return Err(anyhow!(
                "`--no-input` cannot complete the manual-code OAuth flow for `{mount}` (it needs a pasted redirect URL); run it interactively"
            ))
            .with_exit_code(ExitCode::AuthRequired);
        },
        LoginRequest::ManualCode(request) => login_manual(&client, request, mount, output).await?,
        LoginRequest::DeviceCode(request) => {
            // `login_device_code` calls this before its own await point, so a
            // borrow of `output` would live long enough; it is cloned anyway
            // to keep every device-code line going through the same owned
            // handle the opener above uses, rather than mixing a borrow and
            // a clone for the same flow.
            let device_output = output.clone();
            client
                .login_device_code(request, move |prompt| {
                    present_device_prompt(&prompt, no_browser, &device_output);
                    async move { Ok(()) }
                })
                .await
                .with_hint(format!("Re-run `omnifs mount reauth {mount}` to retry"))?
        },
    };
    // Every flow settles the same receipt row on success, in one place,
    // rather than device-code alone printing it inside its own arm.
    output.ledger_row(
        &crate::ui::render::LedgerRow::new(crate::ui::style::Glyph::Done, "oauth", "authorized"),
        key_width,
    );
    Ok(entry)
}

async fn login_manual(
    client: &OAuthClient,
    request: ManualCodeLoginRequest,
    mount: &str,
    output: &crate::ui::output::Output,
) -> anyhow::Result<CredentialEntry> {
    let result = client
        .login_manual_code(request, |url| {
            output.narrate(format!("Open {url}"));
            async move {
                let prompt_output = output.clone();
                let pasted = tokio::task::spawn_blocking(move || {
                    crate::ui::prompt::Text::new("Paste redirect URL or `code state`")
                        .ask_with_output(&prompt_output)
                })
                .await
                .unwrap_or_else(|error| Err(anyhow::anyhow!("prompt task panicked: {error}")))
                .map_err(|error| {
                    if crate::ui::prompt::is_canceled(&error) {
                        omnifs_auth::AuthError::BrowserOpen(MANUAL_PROMPT_CANCELED.to_string())
                    } else {
                        omnifs_auth::AuthError::BrowserOpen(error.to_string())
                    }
                })?;
                manual_code_from_input(&pasted)
                    .map_err(|error| omnifs_auth::AuthError::BrowserOpen(error.to_string()))
            }
        })
        .await;
    match result {
        Err(omnifs_auth::AuthError::BrowserOpen(message)) if message == MANUAL_PROMPT_CANCELED => {
            Err(anyhow::Error::new(crate::ui::prompt::Canceled))
        },
        result => result.with_hint(format!("Re-run `omnifs mount reauth {mount}` to retry")),
    }
}

/// Present the device-code prompt through `Output` narration, so it honors
/// quiet and structured mode like every other line an invocation prints.
/// `output` is an owned clone rather than a borrow: the caller passes this
/// function into a `move` closure that outlives the borrow it would
/// otherwise need.
fn present_device_prompt(
    prompt: &DeviceCodePrompt,
    no_browser: bool,
    output: &crate::ui::output::Output,
) {
    let url = prompt
        .verification_uri_complete
        .as_deref()
        .unwrap_or(&prompt.verification_uri);
    let stream = style::Stream::Stderr;
    output.narrate(crate::ui::style::accent(url, stream));

    // Clipboard copy is best effort only. Failure must not prevent showing
    // the code or continuing the flow.
    let code_line =
        match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(prompt.user_code.clone())) {
            Ok(()) => format!(
                "{} {}",
                crate::ui::style::bold(&prompt.user_code, stream),
                crate::ui::style::dim("(copied to clipboard)", stream)
            ),
            Err(_) => crate::ui::style::bold(&prompt.user_code, stream),
        };
    output.narrate(code_line);

    // Show the code lifetime so the user knows how long they have before the
    // prompt on the provider side expires.
    let secs = prompt.expires_in.as_secs();
    let expiry_text = if secs < 60 {
        format!("expires in {secs}s")
    } else {
        let mins = secs / 60;
        format!("expires in {mins}m")
    };
    output.narrate(crate::ui::style::dim(expiry_text, stream));

    // Only attempt browser open when allowed and a complete uri is present.
    // Report outcome only on real success so we never overstate what happened.
    if !no_browser && let Some(complete_url) = &prompt.verification_uri_complete {
        match webbrowser::open(complete_url) {
            Ok(()) => {
                output.narrate(crate::ui::style::dim("(opened your browser)", stream));
            },
            Err(_) => {
                output.narrate(crate::ui::style::dim(
                    "(could not open a browser; visit the URL above)",
                    stream,
                ));
            },
        }
    }

    output.narrate(crate::ui::style::dim("waiting for confirmation", stream));
}

fn print_oauth_consent_summary(
    output: &crate::ui::output::Output,
    request: &OAuthRequest,
    guidance: &SchemeGuidance,
) {
    let stream = style::Stream::Stderr;
    let scheme = request.scheme();
    output.narrate(crate::ui::style::dim(
        super::explain::experience(&scheme.flow),
        stream,
    ));
    if !guidance.setup_steps.is_empty() {
        output.narrate(crate::ui::style::dim("Guidance:", stream));
        for (index, step) in guidance.setup_steps.iter().enumerate() {
            output.narrate(format!("{}. {step}", index + 1));
        }
    }
    if let Some(url) = &guidance.docs_url {
        output.narrate(format!(
            "{} {}",
            style::dim("Docs:", stream),
            style::accent(url, stream)
        ));
    }
    output.narrate(format!(
        "{} {}",
        style::dim("Scopes:", stream),
        format_scopes(&scheme.default_scopes)
    ));
    if !scheme.inject_domains.is_empty() {
        output.narrate(format!(
            "{} {}",
            style::dim("Applies to:", stream),
            scheme.inject_domains.join(", ")
        ));
    }
}

fn manual_code_from_input(input: &str) -> anyhow::Result<ManualCode> {
    let trimmed = input.trim();
    if let Ok(url) = reqwest::Url::parse(trimmed) {
        let params: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        let code = params
            .get("code")
            .ok_or_else(|| anyhow!("redirect URL does not contain `code`"))?;
        let state = params
            .get("state")
            .ok_or_else(|| anyhow!("redirect URL does not contain `state`"))?;
        return Ok(ManualCode::new(code, state));
    }
    let mut parts = trimmed.split_ascii_whitespace();
    let code = parts.next().ok_or_else(|| anyhow!("missing code"))?;
    let state = parts.next().ok_or_else(|| anyhow!("missing state"))?;
    if parts.next().is_some() {
        anyhow::bail!("expected redirect URL or `code state`");
    }
    Ok(ManualCode::new(code, state))
}

/// Prints each URL the flow would otherwise have opened a browser to, at the
/// moment it would have opened it, rather than buffering for a drain later
/// (by the time a caller could drain a buffer, the flow has already moved
/// on, and the printed URL is stale advice).
struct PrintOpener {
    output: crate::ui::output::Output,
}

impl UrlOpener for PrintOpener {
    fn open<'a>(
        &'a self,
        url: &'a reqwest::Url,
    ) -> Pin<Box<dyn Future<Output = Result<(), omnifs_auth::AuthError>> + Send + 'a>> {
        Box::pin(async move {
            self.output.narrate(format!("Open {url}"));
            Ok(())
        })
    }
}

fn format_scopes(scopes: &[String]) -> String {
    if scopes.is_empty() {
        "<none>".to_owned()
    } else {
        scopes.join(", ")
    }
}
