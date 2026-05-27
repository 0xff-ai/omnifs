//! OAuth login flow.

use crate::error::WithHint;
use anyhow::anyhow;
use omnifs_auth::{
    DeviceCodePrompt, LoginRequest, ManualCode, OAuthClient, OAuthRequest, UrlOpener,
};
use omnifs_creds::CredentialStore;
use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use super::shared::{format_scopes, oauth_request};
use crate::app_context::AppContext;
use crate::catalog::ProviderCatalog;
use crate::paths::{PathOverrides, Paths};
use crate::session::CredsBackend;

pub(super) async fn login(
    _paths: &Paths,
    catalog: &ProviderCatalog,
    store: Box<dyn CredentialStore>,
    mount: &str,
    account: Option<&str>,
    no_browser: bool,
    scopes: &[String],
) -> anyhow::Result<()> {
    let (_mount, request, target) = oauth_request(catalog, mount, account, scopes)?;
    print_oauth_consent_summary(mount, &request);
    let client = OAuthClient::new()?;
    let entry = match request.into_login_request() {
        LoginRequest::Loopback(request) if no_browser => client
            .with_opener(Arc::new(PrintOpener))
            .login_loopback(request)
            .await
            .with_hint(format!("Re-run `omnifs auth login {mount}` to retry"))?,
        LoginRequest::Loopback(request) => client
            .with_system_browser()
            .login_loopback(request)
            .await
            .with_hint(format!("Re-run `omnifs auth login {mount}` to retry"))?,
        LoginRequest::ManualCode(request) => client
            .login_manual_code(request, |url| async move {
                anstream::println!("Open {url}");
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
            .with_hint(format!("Re-run `omnifs auth login {mount}` to retry"))?,
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
            result.with_hint(format!("Re-run `omnifs auth login {mount}` to retry"))?
        },
    };
    for key in target.keys() {
        store.put(key, &entry)?;
    }
    anstream::println!(
        "Stored OAuth credential for `{mount}` with scopes: {}",
        format_scopes(entry.scopes())
    );
    if mount == "github" && entry.scopes().is_empty() {
        anstream::println!(
            "GitHub granted no scopes. Public resources will work; rerun with `--scope repo` for private repositories."
        );
    }
    Ok(())
}

pub(crate) async fn login_with_paths(
    mounts_dir: PathBuf,
    providers_dir: PathBuf,
    credentials_file: PathBuf,
    mount: &str,
    account: Option<&str>,
    no_browser: bool,
    scopes: &[String],
) -> anyhow::Result<()> {
    let ctx = AppContext::resolve(
        PathOverrides {
            mounts_dir: Some(mounts_dir),
            providers_dir: Some(providers_dir),
            ..Default::default()
        },
        None,
        None,
    )?;
    let mut paths = ctx.paths().clone();
    paths.credentials_file = credentials_file;
    let store = CredsBackend::auto(&paths.credentials_file, true);
    login(
        &paths,
        ctx.catalog(),
        store,
        mount,
        account,
        no_browser,
        scopes,
    )
    .await
}

pub(super) fn present_device_prompt(
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
    if !no_browser && let Some(complete_url) = &prompt.verification_uri_complete {
        webbrowser::open(complete_url).ok();
        bar.println(format!("  {}", crate::style::dim("(opened your browser)")));
    }

    bar.set_message("Authorizing — waiting for confirmation");
}

pub(super) fn print_oauth_consent_summary(mount: &str, request: &OAuthRequest) {
    anstream::println!(
        "Requesting OAuth for `{mount}` using scheme `{}`",
        request.scheme().key
    );
    anstream::println!(
        "Scopes: {}",
        format_scopes(&request.scheme().default_scopes)
    );
    if !request.scheme().inject_domains.is_empty() {
        anstream::println!("Applies to: {}", request.scheme().inject_domains.join(", "));
    }
}

pub(super) fn manual_code_from_input(input: &str) -> anyhow::Result<ManualCode> {
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
            anstream::println!("Open {url}");
            Ok(())
        })
    }
}
