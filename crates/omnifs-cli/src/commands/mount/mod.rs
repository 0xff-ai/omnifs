//! `omnifs mount` — add, list, re-authenticate, revoke, or remove mounts.

pub(crate) mod add;
pub(crate) mod auth_import;
pub(crate) mod create;
pub(crate) mod detect;
pub(crate) mod provider_selection;
pub(crate) mod revoke;
pub(crate) mod spec_creation;
mod token_validation;
mod update;

pub(crate) use add::AddArgs;
pub(crate) use add::{render_consent_block, run_static_token_init};
pub(crate) use auth_import::AuthImportDecision;
pub(crate) use auth_import::ImportOutcome;
pub(crate) use create::{MountInitStatus, ReceiptStyle, configure_mount};
pub(crate) use revoke::RevokeArgs;

use anyhow::{Context, anyhow};
use clap::{Args, Subcommand};
use omnifs_api::{
    CredentialKey, CredentialMaterial, CredentialSubmission, MountOpResult, MountRecord,
    MutationOpResult,
};
use omnifs_core::MountName;
use secrecy::ExposeSecret as _;

use crate::error::{ExitCode, WithExitCode};
use crate::mutation::PlannedOp;
use crate::token_source::TokenSource;
use crate::ui::consent::{Decision, Outcome, Plan, Row};
use crate::ui::output::{Output, PromptMode, ResultVerdict};

#[derive(Args, Debug, Clone)]
pub struct MountArgs {
    #[command(subcommand)]
    pub command: MountCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub enum MountCommand {
    /// Add and authenticate a mount.
    Add(AddArgs),
    /// List configured mounts with their provider and auth state.
    Ls,
    /// Show one configured mount and every derived filesystem access path.
    Show(ShowArgs),
    /// Update selected fields in an existing mount.
    Update(update::UpdateArgs),
    /// Re-authenticate an existing mount.
    Reauth(ReauthArgs),
    /// Revoke the configured credential for an existing mount.
    Revoke(RevokeArgs),
    /// Remove a mount config.
    Rm {
        name: String,
        /// Print the removal plan without changing daemon state.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Args, Debug, Clone)]
pub struct ShowArgs {
    /// Existing mount name.
    pub name: String,
}

#[derive(Args, Debug, Clone)]
pub struct ReauthArgs {
    /// Existing mount name to re-authenticate.
    pub name: String,
    /// Print the OAuth URL instead of opening a browser.
    #[arg(long)]
    pub no_browser: bool,
    /// Read the static token from this source. Use `-` for stdin.
    #[arg(long, conflicts_with = "token_env")]
    pub token: Option<String>,
    /// Read the static token from this environment variable.
    #[arg(long, value_name = "ENV_VAR", conflicts_with = "token")]
    pub token_env: Option<String>,
    /// Store the static token without the provider's upstream validation
    /// probe (for CI or restricted tokens that fail the probe endpoint but
    /// work for their intended scope).
    #[arg(long)]
    pub no_validate: bool,
    /// OAuth scope to request. Repeat for multiple scopes.
    #[arg(long = "scope")]
    pub scopes: Vec<String>,
}

fn effective_oauth_scopes(explicit: &[String], prior: &[String]) -> Vec<String> {
    if explicit.is_empty() {
        prior.to_vec()
    } else {
        explicit.to_vec()
    }
}

impl MountArgs {
    pub async fn run(self, output: Output) -> anyhow::Result<ExitCode> {
        match self.command {
            MountCommand::Add(args) => args.run(output).await,
            MountCommand::Ls => ls(output).await,
            MountCommand::Show(args) => show(&args, output).await,
            MountCommand::Update(args) => args.run(output).await,
            MountCommand::Reauth(args) => {
                let receipt = args.run(output.clone()).await?;
                if output.is_structured() {
                    output.emit_result(receipt.verdict, receipt)?;
                }
                Ok(ExitCode::Success)
            },
            MountCommand::Revoke(args) => {
                let receipt = args.run(output.clone()).await?;
                if output.is_structured() {
                    output.emit_result(ResultVerdict::Ok, receipt)?;
                }
                Ok(ExitCode::Success)
            },
            MountCommand::Rm { name, dry_run } => {
                let receipt = rm_with_options(&name, dry_run, &output).await?;
                if output.is_structured() {
                    output.emit_result(receipt.verdict, &receipt)?;
                }
                Ok(ExitCode::Success)
            },
        }
    }
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct MountsResult {
    mounts: Vec<MountRecord>,
    verdict: ResultVerdict,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct MountShowResult {
    mount: MountRecord,
    verdict: ResultVerdict,
}

async fn ls(output: Output) -> anyhow::Result<ExitCode> {
    let result = list_with_output().await?;
    let exit_code = match result.verdict {
        ResultVerdict::Ok => ExitCode::Success,
        ResultVerdict::Degraded => ExitCode::Degraded,
    };
    if output.is_structured() {
        output.emit_result(result.verdict, &result)?;
    } else {
        crate::ui::print_raw(&render_mounts(&result));
    }
    Ok(exit_code)
}

async fn show(args: &ShowArgs, output: Output) -> anyhow::Result<ExitCode> {
    let result = show_with_output(&args.name).await?;
    if output.is_structured() {
        output.emit_result(result.verdict, &result)?;
    } else {
        crate::ui::print_raw(&render_mount_show(&result));
    }
    Ok(match result.verdict {
        ResultVerdict::Ok => ExitCode::Success,
        ResultVerdict::Degraded => ExitCode::Degraded,
    })
}

pub(crate) async fn list_with_output() -> anyhow::Result<MountsResult> {
    let mounts = crate::rpc::RpcClient::resolve()?.list_mounts().await?;
    let verdict = mounts_verdict(&mounts);
    Ok(MountsResult { mounts, verdict })
}

pub(crate) async fn show_with_output(name: &str) -> anyhow::Result<MountShowResult> {
    let name = MountName::new(name.to_owned())?;
    let mount = crate::rpc::RpcClient::resolve()?
        .get_mount(name.clone())
        .await?
        .ok_or_else(|| anyhow!("no mount named `{name}`"))?;
    let verdict = mount_health_verdict(&mount.health);
    Ok(MountShowResult { mount, verdict })
}

fn render_mounts(result: &MountsResult) -> String {
    use crate::ui::table::{
        Block, Cell, Column, Priority, Report, ResourceRow, ResourceTable, WidthPolicy,
    };
    let mut table = ResourceTable::new(
        "Mounts",
        format!("{} configured", result.mounts.len()),
        vec![
            Column::new("Mount", Priority::Identity, WidthPolicy::Auto),
            Column::new("Provider", Priority::Essential, WidthPolicy::Auto),
            Column::new("Auth", Priority::Secondary, WidthPolicy::Auto),
            Column::new("Revision", Priority::Secondary, WidthPolicy::Auto),
            Column::new("Health", Priority::Essential, WidthPolicy::Auto),
        ],
    );
    for mount in &result.mounts {
        let state = health_state(&mount.health);
        table.push(ResourceRow::new(
            [
                Cell::new(mount.definition.name.to_string()),
                Cell::new(provider_label(&mount.provider)),
                Cell::new(auth_label(mount.definition.auth.as_ref())),
                Cell::new(mount.revision.get().to_string()),
                Cell::state(state.clone()),
            ],
            state,
        ));
    }
    let mut report = Report::new();
    report.push(Block::Resources(table));
    report.render()
}

/// `mount show` renders one daemon-owned mount as a detail card.
fn render_mount_show(result: &MountShowResult) -> String {
    use crate::ui::table::{Block, ContextStrip, Meta, Report};
    let mount = &result.mount;
    let mut report = Report::new();
    report.push(Block::Context(
        ContextStrip::new(
            mount.definition.name.to_string(),
            String::new(),
            health_state(&mount.health),
        )
        .with_metadata([
            Meta::new("provider", provider_label(&mount.provider)),
            Meta::new("revision", mount.revision.get().to_string()),
            Meta::new("version", mount.version.to_string()),
        ]),
    ));
    let mut facts = vec![
        ("provider", provider_label(&mount.provider)),
        ("pin", mount.provider.id.to_string()),
        ("revision", mount.revision.get().to_string()),
        ("version", mount.version.to_string()),
        ("auth", auth_label(mount.definition.auth.as_ref())),
        ("health", health_label(&mount.health)),
    ];
    if let Some(limits) = mount.definition.limits.as_ref() {
        facts.push(("limits", format_limits(limits)));
    }
    if let Some(config) = config_summary(&mount.definition.config) {
        facts.push(("config", config));
    }
    let mut card = report.render();
    card.push_str(&detail_rows(&facts));
    card.push('\n');
    card
}

/// Compute the overall verdict from daemon-reported mount health.
fn mounts_verdict(mounts: &[MountRecord]) -> ResultVerdict {
    if mounts
        .iter()
        .any(|mount| !matches!(&mount.health, omnifs_api::MountHealth::Active))
    {
        ResultVerdict::Degraded
    } else {
        ResultVerdict::Ok
    }
}

fn mount_health_verdict(health: &omnifs_api::MountHealth) -> ResultVerdict {
    if matches!(health, omnifs_api::MountHealth::Active) {
        ResultVerdict::Ok
    } else {
        ResultVerdict::Degraded
    }
}

fn provider_label(provider: &omnifs_api::ProviderReference) -> String {
    provider.version.as_ref().map_or_else(
        || provider.name.clone(),
        |version| format!("{}@{version}", provider.name),
    )
}

fn auth_label(auth: Option<&omnifs_api::MountCredential>) -> String {
    auth.map_or_else(
        || "none".to_owned(),
        |auth| format!("{} ({})", auth.scheme, auth.account_label),
    )
}

fn health_label(health: &omnifs_api::MountHealth) -> String {
    match health {
        omnifs_api::MountHealth::Active => "active".to_owned(),
        omnifs_api::MountHealth::AuthRequired => "auth required".to_owned(),
        omnifs_api::MountHealth::ProviderUnavailable { reason } => {
            format!("provider unavailable: {reason}")
        },
        omnifs_api::MountHealth::Failed { reason } => format!("failed: {reason}"),
    }
}

fn health_state(health: &omnifs_api::MountHealth) -> crate::ui::table::StateToken {
    match health {
        omnifs_api::MountHealth::Active => crate::ui::table::StateToken::positive("active"),
        omnifs_api::MountHealth::AuthRequired => {
            crate::ui::table::StateToken::attention("auth required")
        },
        omnifs_api::MountHealth::ProviderUnavailable { .. }
        | omnifs_api::MountHealth::Failed { .. } => {
            crate::ui::table::StateToken::failure(health_label(health))
        },
    }
}

fn format_limits(limits: &omnifs_api::MountLimits) -> String {
    let mut values = Vec::new();
    if let Some(value) = limits.max_memory_mb {
        values.push(format!("memory={value}MB"));
    }
    if let Some(value) = limits.max_fetch_blob_bytes {
        values.push(format!("fetch={value}B"));
    }
    if values.is_empty() {
        "none".to_owned()
    } else {
        values.join(", ")
    }
}

fn config_summary(config: &[u8]) -> Option<String> {
    let value = serde_json::from_slice::<serde_json::Value>(config).ok()?;
    let object = value.as_object()?;
    if object.is_empty() {
        return None;
    }
    Some(
        object
            .iter()
            .map(|(key, value)| format!("{key}: {value}"))
            .collect::<Vec<_>>()
            .join(", "),
    )
}

/// One line per fact, two-space indented, no glyph column.
fn detail_rows(facts: &[(&'static str, String)]) -> String {
    use std::fmt::Write as _;

    let key_width = facts
        .iter()
        .map(|(key, _)| key.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for (key, value) in facts {
        let pad = key_width.saturating_sub(key.chars().count()) + 3;
        let _ = writeln!(out, "  {key}{}{value}", " ".repeat(pad));
    }
    out
}

impl ReauthArgs {
    async fn run(
        self,
        output: Output,
    ) -> anyhow::Result<crate::commands::receipt::MountReauthReceipt> {
        crate::commands::daemon_start::start().await?;
        let prompt = output.prompt_mode();
        let result = self.run_with_output(&output, prompt).await;
        if result.is_ok() {
            output.outro(format!("Re-authenticated `{}`.", self.name));
        }
        // `result?` propagates any reauthentication failure before this
        // point, so reaching the constructor already proves success; there
        // is no outcome here for a verdict to be derived from.
        result?;
        Ok(crate::commands::receipt::MountReauthReceipt {
            verdict: ResultVerdict::Ok,
            mount: self.name.clone(),
        })
    }

    #[allow(clippy::too_many_lines)] // one linear reauthentication flow
    pub(crate) async fn run_with_output(
        &self,
        output: &crate::ui::output::Output,
        prompt: PromptMode,
    ) -> anyhow::Result<()> {
        let mount_name = self.name.as_str();
        let rpc = crate::rpc::RpcClient::resolve()?;
        let name = MountName::new(mount_name.to_owned())?;
        let record = rpc.get_mount(name.clone()).await?.ok_or_else(|| {
            anyhow!("no mount named `{mount_name}`; run `omnifs mount add <provider>` to create it")
        })?;
        let Some(auth) = record.definition.auth.as_ref() else {
            anyhow::bail!("mount `{mount_name}` needs no authentication");
        };
        let key = CredentialKey {
            provider_name: record.provider.name.clone(),
            scheme: auth.scheme.clone(),
            account_label: auth.account_label.clone(),
        };
        let state = crate::client_state::ClientState::resolve()?;
        let status = rpc.credential_status(key.clone()).await?;
        let metadata = rpc
            .provider_metadata(record.definition.provider)
            .await?
            .ok_or_else(|| anyhow!("provider metadata is unavailable for `{mount_name}`"))?;
        let manifest = omnifs_provider::ProviderManifest::from_bytes(&metadata.manifest)
            .context("parse daemon provider metadata")?;
        let auth_manifest = manifest
            .auth
            .as_ref()
            .map(omnifs_provider::ProviderAuthManifest::wasm_auth_manifest);
        let selected = crate::auth::Auth::from_scheme(
            auth_manifest.as_ref(),
            &auth.scheme,
            Some(auth.account_label.clone()),
        )?;

        // `--no-input` must never reach an OAuth browser handoff (it would hang
        // on the browser confirm or the manual-code paste). Mirror the add-side
        // guard: bail naming the interactive and static-token alternatives.
        let interactive = prompt.interactive();
        if !interactive && selected.is_oauth() {
            return Err(anyhow!(
                "`omnifs mount reauth {mount_name}` cannot complete OAuth without a terminal; run it interactively, or use a static-token scheme with --token - or --token-env VAR"
            ))
            .with_exit_code(ExitCode::AuthRequired);
        }

        // `reauth`'s own auth-outcome block shares the same key set `mount
        // add` uses for its completed-auth rows (`oauth`/`signed in`/
        // `credential`), since both flows route through the same
        // `login`/`run_static_token_init` primitives.
        let submission = if selected.is_oauth() {
            let requested_scopes = effective_oauth_scopes(
                &self.scopes,
                status.as_ref().map_or(&[][..], |status| &status.scopes),
            );
            output.narrate(format!("re-authenticating `{mount_name}` over OAuth"));
            crate::auth::login::login_for_submission(
                record.definition.provider,
                &manifest,
                &selected,
                &auth.account_label,
                crate::auth::LoginInteractivity {
                    no_browser: self.no_browser,
                    no_input: prompt.no_input(),
                    scopes: Some(&requested_scopes),
                },
                output,
                crate::auth::auth_receipt_key_width(),
            )
            .await?
        } else {
            let source = TokenSource::resolve(
                self.token.as_deref(),
                self.token_env.as_deref(),
                interactive,
            )?;
            let token = source.read(output)?;
            if !self.no_validate {
                let scheme = selected.static_token_scheme(&manifest)?;
                if let Some(validation) = scheme.validation.as_ref() {
                    token_validation::validate_static_token(
                        validation,
                        scheme.header_name.as_deref().unwrap_or("Authorization"),
                        &scheme.value_prefix,
                        token.expose_secret(),
                        output,
                    )
                    .await?;
                }
            }
            CredentialSubmission {
                provider: record.definition.provider,
                scheme: auth.scheme.clone(),
                account_label: auth.account_label.clone(),
                material: CredentialMaterial::StaticToken {
                    token: omnifs_api::SecretBytes::new(token.expose_secret().as_bytes().to_vec()),
                },
                overrides: omnifs_api::CredentialClientOverrides {
                    client_id: None,
                    client_secret: None,
                    redirect_uri: None,
                    scopes: None,
                },
            }
        };
        if let Some(outcome) = crate::mutation::run(&rpc, &state, output, || async move {
            Ok(vec![PlannedOp::submit_credential(&key, submission)])
        })
        .await?
        {
            crate::mutation::narrate_serving(output, &outcome.serving);
        }
        output.ledger_row(
            &crate::ui::render::LedgerRow::new(
                crate::ui::style::Glyph::Done,
                format!("credential `{}/{}`", auth.scheme, auth.account_label),
                "stored in daemon",
            ),
            crate::auth::auth_receipt_key_width(),
        );
        Ok(())
    }
}

async fn rm_with_options(
    name: &str,
    dry_run: bool,
    output: &Output,
) -> anyhow::Result<crate::commands::receipt::MountRemoveReceipt> {
    let output = output.clone();
    let name =
        MountName::new(name.to_owned()).with_context(|| format!("invalid mount name `{name}`"))?;
    let rpc = crate::rpc::RpcClient::resolve()?;
    let mount = rpc.get_mount(name.clone()).await?;
    let plan = mount_remove_plan(&name, mount.as_ref());
    output.plan(&plan);
    if mount.is_none() {
        return remove_absent_mount(&rpc, &name, plan, dry_run, &output).await;
    }
    match Decision::resolve(output.prompt_mode(), dry_run, "Remove?", "-y", &output)? {
        Decision::DryRun => {
            output.outro("Dry run, nothing changed.");
            return Ok(crate::commands::receipt::MountRemoveReceipt::dry_run(
                name.to_string(),
                plan,
            ));
        },
        Decision::Apply => {},
    }
    let state = crate::client_state::ClientState::resolve()?;
    let outcome = crate::mutation::run(&rpc, &state, &output, {
        let name = name.clone();
        || async move { Ok(vec![PlannedOp::mount_remove(name)]) }
    })
    .await?
    .context("mount removal produced no result")?;
    crate::mutation::narrate_serving(&output, &outcome.serving);
    let mount_result = outcome
        .results
        .into_iter()
        .find_map(|result| match result {
            MutationOpResult::Mount(mount) => Some(mount),
            MutationOpResult::Credential(_) => None,
        })
        .context("mount removal batch did not include a mount result")?;
    Ok(finish_mount_removal(&output, plan, &mount_result))
}

async fn remove_absent_mount(
    rpc: &crate::rpc::RpcClient,
    name: &MountName,
    plan: Plan,
    dry_run: bool,
    output: &Output,
) -> anyhow::Result<crate::commands::receipt::MountRemoveReceipt> {
    if dry_run {
        output.outro("Dry run, nothing changed.");
        return Ok(crate::commands::receipt::MountRemoveReceipt::dry_run(
            name.to_string(),
            plan,
        ));
    }
    // Settle whatever a prior interrupted command left in the journal: the
    // read above already proved this mount absent, which is all a
    // `mount.remove` batch's provenance check needs either way.
    let state = crate::client_state::ClientState::resolve()?;
    crate::mutation::settle(rpc, &state, output).await?;
    let receipt = plan.receipt([Outcome::skip("mount", "already absent")]);
    output.receipt(&receipt);
    output.outro(format!("Mount `{name}` already absent."));
    Ok(crate::commands::receipt::MountRemoveReceipt::applied(
        name.to_string(),
        plan,
        receipt.rows,
        None,
    ))
}

fn mount_remove_plan(name: &MountName, mount: Option<&MountRecord>) -> Plan {
    let mut plan = Plan::new(format!("Removing mount `{name}`"));
    match mount {
        Some(mount) => plan.push(Row::remove(
            "mount",
            "mount",
            format!(
                "{name} (revision {}, version {})",
                mount.revision.get(),
                mount.version
            ),
        )),
        None => plan.push(Row::keep(
            "mount",
            "mount",
            format!("{name} (already absent)"),
        )),
    }
    plan
}

fn finish_mount_removal(
    output: &Output,
    plan: Plan,
    result: &MountOpResult,
) -> crate::commands::receipt::MountRemoveReceipt {
    let receipt = plan.receipt([Outcome::done("mount", "removed")]);
    output.receipt(&receipt);
    output.outro(format!(
        "Removed `{}` at revision {}.",
        result.name,
        result.revision.get()
    ));
    crate::commands::receipt::MountRemoveReceipt::applied(
        result.name.to_string(),
        plan,
        receipt.rows,
        Some(result.revision.get()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reauth_keeps_granted_scopes_without_an_explicit_override() {
        let prior = vec!["repo".to_owned(), "read:user".to_owned()];
        assert_eq!(effective_oauth_scopes(&[], &prior), prior);
        assert_eq!(
            effective_oauth_scopes(&["gist".to_owned()], &prior),
            vec!["gist"]
        );
    }

    #[test]
    fn removal_plan_names_the_daemon_mount_row() {
        let name = MountName::try_from("github").unwrap();
        let plan = mount_remove_plan(&name, None);
        assert_eq!(plan.rows[0].id, "mount");
        assert_eq!(plan.rows[0].value, "github (already absent)");
        assert_eq!(plan.title, "Removing mount `github`");
    }

    /// `omnifs mount ls` renders exactly the Mounts section of the status
    /// report, never the context strip or
    /// the Filesystems table alongside it.
    #[test]
    fn render_mounts_is_exactly_the_status_mounts_section() {
        let mounts = vec![daemon_mount(omnifs_api::MountHealth::Active)];
        let result = MountsResult {
            mounts: mounts.clone(),
            verdict: ResultVerdict::Ok,
        };
        let rendered = render_mounts(&result);
        assert!(rendered.contains("Mounts"));
        assert!(rendered.contains("github"));
        assert!(!rendered.contains("Filesystems"), "{rendered:?}");
        assert!(rendered.contains("active"));
    }

    fn daemon_mount(health: omnifs_api::MountHealth) -> MountRecord {
        MountRecord {
            definition: omnifs_api::MountDefinition {
                name: MountName::try_from("github").unwrap(),
                provider: omnifs_core::ProviderId::from_digest([0x12; 32]),
                auth: Some(omnifs_api::MountCredential {
                    scheme: "oauth".to_owned(),
                    account_label: "work".to_owned(),
                }),
                limits: None,
                config: br#"{"org":"raulk"}"#.to_vec(),
            },
            provider: omnifs_api::ProviderReference {
                id: omnifs_core::ProviderId::from_digest([0x12; 32]),
                name: "github".to_owned(),
                version: Some("0.3.2".to_owned()),
            },
            version: omnifs_core::MountVersion::from_digest([0x34; 32]),
            revision: omnifs_core::MountRevision::new(7),
            health,
            auth_health: Some(omnifs_api::CredentialHealth::Ready),
            last_mutation_id: omnifs_core::MutationId::from_bytes([0x56; 16]),
        }
    }

    fn show_result(mount: MountRecord) -> MountShowResult {
        MountShowResult {
            mount,
            verdict: ResultVerdict::Ok,
        }
    }

    fn healthy_mount() -> MountRecord {
        daemon_mount(omnifs_api::MountHealth::Active)
    }

    /// `mount show` is a detail card, never the tabular
    /// single-row table `render_mounts` (`mount ls`) already owns.
    #[test]
    fn render_mount_show_is_a_detail_card_not_a_table() {
        let rendered = render_mount_show(&show_result(healthy_mount()));
        let lines = rendered.lines().collect::<Vec<_>>();
        assert!(lines[0].starts_with("github"), "{rendered:?}");
        assert!(lines[0].contains("active"), "{rendered:?}");
        assert!(!rendered.contains("Mounts"), "{rendered:?}");
        assert!(rendered.contains("oauth (work)"), "{rendered:?}");
        assert!(rendered.contains("revision"), "{rendered:?}");

        let config_line = lines
            .iter()
            .find(|line| line.trim_start().starts_with("config"))
            .expect("config row");
        assert!(config_line.contains(r#"org: "raulk""#), "{rendered:?}");
    }

    #[test]
    fn degraded_mounts_degrade_verdict() {
        assert_eq!(
            mounts_verdict(&[daemon_mount(omnifs_api::MountHealth::AuthRequired)]),
            ResultVerdict::Degraded
        );
    }
}
