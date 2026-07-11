#![allow(clippy::disallowed_macros)] // migrates in wave 3 (cli-redesign)
//! OAuth login flow.

use crate::error::{ExitCode, WithExitCode, WithHint};
use anyhow::anyhow;
use omnifs_auth::{
    CredentialService, DeviceCodePrompt, LoginRequest, ManualCode, OAuthClient, OAuthRequest,
    UrlOpener,
};
use omnifs_workspace::creds::{CredentialStore, FileStore};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use super::explain::{self, AuthMode};
use crate::credential_target::CredentialTarget;
use crate::style;
use crate::workspace::Workspace;
use omnifs_workspace::authn::SchemeGuidance;
use omnifs_workspace::provider::Catalog;

/// Whether to suppress the system browser and whether prompts are allowed.
/// Bundled so `login` keeps a readable argument list.
#[derive(Clone, Copy)]
struct LoginInteractivity {
    no_browser: bool,
    no_input: bool,
}

async fn login(
    catalog: &Catalog,
    mounts: &[crate::mount_config::MountConfig],
    store: Box<dyn CredentialStore>,
    mount: &str,
    account: Option<&str>,
    interactivity: LoginInteractivity,
    scopes: &[String],
) -> anyhow::Result<CredentialTarget> {
    let LoginInteractivity {
        no_browser,
        no_input,
    } = interactivity;
    let mount_auth = crate::auth::MountAuth::load(catalog, mounts, mount)?;
    let (request, target) = mount_auth.oauth_request(account, scopes)?;
    let guidance = omnifs_workspace::mounts::pinned_manifest(catalog, mount_auth.spec())
        .ok()
        .flatten()
        .and_then(|manifest| manifest.auth)
        .map(|auth| auth.guidance_for(&request.scheme().key))
        .unwrap_or_default();
    print_oauth_consent_summary(mount, &request, &guidance);
    let client = OAuthClient::new()?;
    let client = if no_browser {
        client.with_opener(Arc::new(PrintOpener))
    } else {
        client.with_system_browser()
    };
    let entry = match request.into_login_request() {
        LoginRequest::Loopback(request) => client
            .login_loopback(request)
            .await
            .with_hint(format!("Re-run `omnifs mounts reauth {mount}` to retry"))?,
        LoginRequest::ClientSideToken(request) => client
            .login_client_side_token(request)
            .await
            .with_hint(format!("Re-run `omnifs mounts reauth {mount}` to retry"))?,
        LoginRequest::ManualCode(_) if no_input => {
            return Err(anyhow!(
                "`--no-input` cannot complete the manual-code OAuth flow for `{mount}` (it needs a pasted redirect URL); run it interactively"
            ))
            .with_exit_code(ExitCode::AuthRequired);
        },
        LoginRequest::ManualCode(request) => client
            .login_manual_code(request, |url| async move {
                anstream::eprintln!("Open {url}");
                let pasted = tokio::task::spawn_blocking(|| {
                    inquire::Text::new("Paste redirect URL or `code state`")
                        .prompt()
                        .map_err(|e| anyhow::anyhow!("{e}"))
                })
                .await
                .unwrap_or_else(|e| Err(anyhow::anyhow!("prompt task panicked: {e}")))
                .map_err(|e| omnifs_auth::AuthError::BrowserOpen(e.to_string()))?;
                manual_code_from_input(&pasted)
                    .map_err(|error| omnifs_auth::AuthError::BrowserOpen(error.to_string()))
            })
            .await
            .with_hint(format!("Re-run `omnifs mounts reauth {mount}` to retry"))?,
        LoginRequest::DeviceCode(request) => {
            let bar = indicatif::ProgressBar::new_spinner();
            bar.set_style(indicatif::ProgressStyle::with_template(
                "{spinner:.cyan} {msg}",
            )?);
            bar.enable_steady_tick(std::time::Duration::from_millis(120));
            let bar_clone = bar.clone();
            let result = client
                .login_device_code(request, move |prompt| {
                    let bar = bar_clone.clone();
                    async move {
                        present_device_prompt(&prompt, &bar, no_browser);
                        Ok(())
                    }
                })
                .await;
            match &result {
                Ok(_) => {
                    bar.finish_with_message(format!("{} Authorized", crate::style::success("✓")));
                },
                Err(_) => bar.finish_and_clear(),
            }
            result.with_hint(format!("Re-run `omnifs mounts reauth {mount}` to retry"))?
        },
    };
    // Write through the credential service so the single store owner records it.
    let service = CredentialService::new(Arc::from(store), OAuthClient::new()?);
    for key in target.keys() {
        service.store_entry(key, entry.clone())?;
    }
    anstream::eprintln!(
        "Stored OAuth credential for `{mount}` with scopes: {}",
        format_scopes(entry.scopes())
    );
    if mount == "github" && entry.scopes().is_empty() {
        anstream::eprintln!(
            "GitHub granted no scopes. Public resources will work; rerun with `--scope repo` for private repositories."
        );
    }
    Ok(target)
}

pub(crate) async fn login_with_workspace(
    workspace: &Workspace,
    mount: &str,
    account: Option<&str>,
    no_browser: bool,
    no_input: bool,
    scopes: &[String],
) -> anyhow::Result<CredentialTarget> {
    let store = Box::new(FileStore::new(&workspace.layout().credentials_file));
    let mounts = workspace.mounts()?;
    login(
        workspace.catalog(),
        &mounts,
        store,
        mount,
        account,
        LoginInteractivity {
            no_browser,
            no_input,
        },
        scopes,
    )
    .await
}

fn present_device_prompt(
    prompt: &DeviceCodePrompt,
    bar: &indicatif::ProgressBar,
    no_browser: bool,
) {
    // Verification URL.
    let url = prompt
        .verification_uri_complete
        .as_deref()
        .unwrap_or(&prompt.verification_uri);
    bar.println(format!("  {}", crate::style::accent(url)));

    // User code — attempt clipboard copy best-effort.
    let code_line =
        match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(prompt.user_code.clone())) {
            Ok(()) => format!(
                "  {} {}",
                crate::style::bold(&prompt.user_code),
                crate::style::dim("(copied to clipboard)")
            ),
            Err(_) => format!("  {}", crate::style::bold(&prompt.user_code)),
        };
    bar.println(code_line);

    // Open the browser when a complete URI is available and not suppressed.
    // Only claim the browser opened when the open actually succeeded; otherwise
    // point at manual entry so the message never overstates what happened.
    if !no_browser && let Some(complete_url) = &prompt.verification_uri_complete {
        match webbrowser::open(complete_url) {
            Ok(()) => bar.println(format!("  {}", crate::style::dim("(opened your browser)"))),
            Err(_) => bar.println(format!(
                "  {}",
                crate::style::dim("(could not open a browser; visit the URL above)")
            )),
        }
    }

    bar.set_message("Authorizing — waiting for confirmation");
}

fn print_oauth_consent_summary(mount: &str, request: &OAuthRequest, guidance: &SchemeGuidance) {
    let scheme = request.scheme();
    let mode = AuthMode::from_oauth_flow(&scheme.flow);
    anstream::eprintln!(
        "Requesting OAuth for `{mount}` using scheme `{}` ({})",
        scheme.key,
        mode.label()
    );
    explain::render_oauth_intro(mode, guidance);
    anstream::eprintln!(
        "  {} {}",
        style::dim("Scopes:"),
        format_scopes(&scheme.default_scopes)
    );
    if !scheme.inject_domains.is_empty() {
        anstream::eprintln!(
            "  {} {}",
            style::dim("Applies to:"),
            scheme.inject_domains.join(", ")
        );
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

struct PrintOpener;

impl UrlOpener for PrintOpener {
    fn open<'a>(
        &'a self,
        url: &'a reqwest::Url,
    ) -> Pin<Box<dyn Future<Output = Result<(), omnifs_auth::AuthError>> + Send + 'a>> {
        Box::pin(async move {
            anstream::eprintln!("Open {url}");
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
