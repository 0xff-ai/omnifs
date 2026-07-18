//! OAuth login flow.

use crate::error::{ExitCode, WithExitCode, WithHint};
use anyhow::anyhow;
use omnifs_auth::{
    DeviceCodePrompt, LoginRequest, ManualCode, ManualCodeLoginRequest, OAuthClient, OAuthRequest,
    UrlOpener,
};
use omnifs_workspace::creds::{CredentialEntry, CredentialStore};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use super::explain::AuthMode;
use crate::credential_target::CredentialTarget;
use crate::ui::style;
use omnifs_workspace::Workspace;
use omnifs_workspace::authn::SchemeGuidance;
use omnifs_workspace::mounts::Spec;
use omnifs_workspace::provider::Catalog;

const MANUAL_PROMPT_CANCELED: &str = "omnifs-manual-oauth-prompt-canceled";

/// Whether to suppress the system browser and whether prompts are allowed.
/// Bundled so `login` and its two public wrappers keep a readable argument
/// list (each would otherwise carry `no_browser`/`no_input`/`scopes` as three
/// separate positional bools/slices).
#[derive(Clone, Copy)]
pub(crate) struct LoginInteractivity<'a> {
    pub(crate) no_browser: bool,
    pub(crate) no_input: bool,
    pub(crate) scopes: &'a [String],
}

async fn login(
    catalog: &Catalog,
    mount_auth: crate::auth::MountAuth,
    store: &dyn CredentialStore,
    account: Option<&str>,
    interactivity: LoginInteractivity<'_>,
    output: &crate::ui::output::Output,
    key_width: usize,
) -> anyhow::Result<CredentialTarget> {
    let LoginInteractivity {
        no_browser,
        no_input,
        scopes,
    } = interactivity;
    let mount = mount_auth.spec().mount.clone();
    let (request, target) = mount_auth.oauth_request(account, scopes)?;
    let guidance = omnifs_workspace::mounts::pinned_manifest(catalog, mount_auth.spec())
        .ok()
        .flatten()
        .and_then(|manifest| manifest.auth)
        .map(|auth| auth.guidance_for(&request.scheme().key))
        .unwrap_or_default();
    let mode = AuthMode::from_oauth_flow(&request.scheme().flow);
    output.note(format!(
        "requesting OAuth for `{mount}` using scheme `{}` ({})",
        request.scheme().key,
        mode.label()
    ));
    print_oauth_consent_summary(output, &request, &guidance);
    let client = OAuthClient::new()?;
    let printed_urls = Arc::new(Mutex::new(Vec::new()));
    let client = if no_browser {
        client.with_opener(Arc::new(PrintOpener {
            urls: Arc::clone(&printed_urls),
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
        LoginRequest::ManualCode(request) => login_manual(&client, request, &mount, output).await?,
        LoginRequest::DeviceCode(request) => {
            // The callback runs before the await inside login_device_code, so we cannot
            // borrow &mut output across the future. `present_device_prompt` prints
            // directly through the raw stderr writer instead.
            let result = client
                .login_device_code(request, move |prompt| {
                    present_device_prompt(&prompt, no_browser);
                    async move { Ok(()) }
                })
                .await;
            if result.is_ok() {
                output.ledger_row(
                    &crate::ui::render::LedgerRow::new(
                        crate::ui::style::Glyph::Done,
                        "oauth",
                        "authorized",
                    ),
                    key_width,
                );
            }
            result.with_hint(format!("Re-run `omnifs mount reauth {mount}` to retry"))?
        },
    };
    for url in printed_urls
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .drain(..)
    {
        output.note(format!("Open {url}"));
    }
    // Store each target key through the workspace store.
    for key in target.keys() {
        store.put(key, &entry)?;
    }
    output.note(format!(
        "stored OAuth credential for `{mount}` with scopes: {}",
        format_scopes(entry.scopes())
    ));
    if mount == "github" && entry.scopes().is_empty() {
        output.note(
            "GitHub granted no scopes. Public resources will work; rerun with `--scope repo` for private repositories.",
        );
    }
    Ok(target)
}

async fn login_manual(
    client: &OAuthClient,
    request: ManualCodeLoginRequest,
    mount: &str,
    output: &crate::ui::output::Output,
) -> anyhow::Result<CredentialEntry> {
    let result = client
        .login_manual_code(request, |url| {
            output.note(format!("Open {url}"));
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

pub(crate) async fn login_with_workspace(
    workspace: &Workspace,
    mount: &str,
    account: Option<&str>,
    interactivity: LoginInteractivity<'_>,
    output: &crate::ui::output::Output,
    key_width: usize,
) -> anyhow::Result<CredentialTarget> {
    let store = workspace.credentials();
    let mounts = crate::mount_config::load_mounts(workspace)?;
    let mount_auth = crate::auth::MountAuth::load(workspace.catalog(), &mounts, mount)?;
    login(
        workspace.catalog(),
        mount_auth,
        store,
        account,
        interactivity,
        output,
        key_width,
    )
    .await
}

/// Authenticate a mount that is still being created from its already-resolved
/// spec. Mount creation must not reload this spec by name: when a live daemon persisted
/// it in another process, this command's mount registry can still hold the
/// snapshot from before the create.
pub(crate) async fn login_with_spec(
    workspace: &Workspace,
    spec: &Spec,
    account: Option<&str>,
    interactivity: LoginInteractivity<'_>,
    output: &crate::ui::output::Output,
    key_width: usize,
) -> anyhow::Result<CredentialTarget> {
    let store = workspace.credentials();
    let mount_auth = crate::auth::MountAuth::from_spec(workspace.catalog(), spec.clone());
    login(
        workspace.catalog(),
        mount_auth,
        store,
        account,
        interactivity,
        output,
        key_width,
    )
    .await
}

fn present_device_prompt(prompt: &DeviceCodePrompt, no_browser: bool) {
    // Printed directly through the `ui` toolkit's raw stderr writer rather
    // than through `Output`: the callback runs before the await inside
    // `login_device_code`, so it cannot borrow `&mut output` across the
    // future (see `login_manual` above for the same constraint spelled out).
    let url = prompt
        .verification_uri_complete
        .as_deref()
        .unwrap_or(&prompt.verification_uri);
    let stream = style::Stream::Stderr;
    crate::ui::eprint_raw(&format!("{}\n", crate::ui::style::accent(url, stream)));

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
    crate::ui::eprint_raw(&format!("{code_line}\n"));

    // Show the code lifetime so the user knows how long they have before the
    // prompt on the provider side expires.
    let secs = prompt.expires_in.as_secs();
    let expiry_text = if secs < 60 {
        format!("expires in {secs}s")
    } else {
        let mins = secs / 60;
        format!("expires in {mins}m")
    };
    crate::ui::eprint_raw(&format!("{}\n", crate::ui::style::dim(expiry_text, stream)));

    // Only attempt browser open when allowed and a complete uri is present.
    // Report outcome only on real success so we never overstate what happened.
    if !no_browser && let Some(complete_url) = &prompt.verification_uri_complete {
        match webbrowser::open(complete_url) {
            Ok(()) => {
                crate::ui::eprint_raw(&format!(
                    "{}\n",
                    crate::ui::style::dim("(opened your browser)", stream)
                ));
            },
            Err(_) => {
                crate::ui::eprint_raw(&format!(
                    "{}\n",
                    crate::ui::style::dim(
                        "(could not open a browser; visit the URL above)",
                        stream,
                    )
                ));
            },
        }
    }

    crate::ui::eprint_raw(&format!(
        "{}\n",
        crate::ui::style::dim("waiting for confirmation", stream)
    ));
}

fn print_oauth_consent_summary(
    output: &crate::ui::output::Output,
    request: &OAuthRequest,
    guidance: &SchemeGuidance,
) {
    let stream = style::Stream::Stderr;
    let scheme = request.scheme();
    let mode = AuthMode::from_oauth_flow(&scheme.flow);
    output.note(crate::ui::style::dim(mode.experience(), stream));
    if !guidance.setup_steps.is_empty() {
        output.note(crate::ui::style::dim("Guidance:", stream));
        for (index, step) in guidance.setup_steps.iter().enumerate() {
            output.note(format!("{}. {step}", index + 1));
        }
    }
    if let Some(url) = &guidance.docs_url {
        output.note(format!(
            "{} {}",
            style::dim("Docs:", stream),
            style::accent(url, stream)
        ));
    }
    output.note(format!(
        "{} {}",
        style::dim("Scopes:", stream),
        format_scopes(&scheme.default_scopes)
    ));
    if !scheme.inject_domains.is_empty() {
        output.note(format!(
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

struct PrintOpener {
    urls: Arc<Mutex<Vec<String>>>,
}

impl UrlOpener for PrintOpener {
    fn open<'a>(
        &'a self,
        url: &'a reqwest::Url,
    ) -> Pin<Box<dyn Future<Output = Result<(), omnifs_auth::AuthError>> + Send + 'a>> {
        Box::pin(async move {
            self.urls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(url.to_string());
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

#[cfg(test)]
mod tests {
    use crate::test_support::{fixture_workspace, install_fixture_provider, spec_with_reference};

    #[test]
    fn planned_spec_constructs_oauth_without_reloading_workspace_mounts() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = fixture_workspace(tmp.path());
        let mounts_dir = tmp.path().join("mounts");
        let providers_dir = tmp.path().join("providers");
        std::fs::create_dir_all(&mounts_dir).unwrap();
        std::fs::create_dir_all(&providers_dir).unwrap();
        let reference = install_fixture_provider(&providers_dir, "planned-oauth");

        assert!(
            crate::mount_config::load_mounts(&workspace)
                .unwrap()
                .is_empty()
        );
        let spec = spec_with_reference(
            &reference,
            r#"{
                "mount": "planned-oauth",
                "auth": { "type": "oauth", "scheme": "device" }
            }"#,
        );

        let mount_auth = crate::auth::MountAuth::from_spec(workspace.catalog(), spec);
        let (request, target) = mount_auth.oauth_request(None, &[]).unwrap();

        assert_eq!(request.scheme().key, "device");
        assert!(target.primary_key().is_some());
        assert!(
            crate::mount_config::load_mounts(&workspace)
                .unwrap()
                .is_empty()
        );
    }
}
