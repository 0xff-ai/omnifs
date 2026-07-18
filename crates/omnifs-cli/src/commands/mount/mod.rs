//! `omnifs mount` — add, list, re-authenticate, revoke, or remove mounts.

pub(crate) mod add;
pub(crate) mod auth_import;
pub(crate) mod detect;
pub(crate) mod mount_file;
pub(crate) mod provider_selection;
pub(crate) mod revoke;
pub(crate) mod spec_creation;
mod token_validation;

pub(crate) use add::AddArgs;
pub(crate) use add::{render_consent_block, run_static_token_init};
pub(crate) use auth_import::AuthImportDecision;
pub(crate) use auth_import::ImportOutcome;
pub(crate) use revoke::RevokeArgs;

use anyhow::{Context, anyhow};
use clap::{Args, Subcommand};
use omnifs_workspace::mounts::Name as MountName;
use std::path::Path;

use crate::credential_target::CredentialTarget;
use crate::error::{ExitCode, WithExitCode};
use crate::stages::PromptMode;
use crate::token_source::TokenSource;
use crate::ui::consent::{Decision, Outcome, Plan, Row};
use crate::ui::output::{Output, ResultVerdict};
use omnifs_workspace::Workspace;

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
    Ls(LsArgs),
    /// Show one configured mount and every derived frontend access path.
    Show(ShowArgs),
    /// Re-authenticate an existing mount.
    Reauth(ReauthArgs),
    /// Revoke the configured credential for an existing mount.
    Revoke(RevokeArgs),
    /// Remove a mount config.
    Rm {
        name: String,
        /// Print the removal plan without changing the workspace.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Args, Debug, Clone, Default)]
pub struct LsArgs {}

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

impl MountArgs {
    pub async fn run(self, output: Output) -> anyhow::Result<ExitCode> {
        match self.command {
            MountCommand::Add(args) => args.run(output).await,
            MountCommand::Ls(args) => ls(&args, output).await,
            MountCommand::Show(args) => show(&args, output).await,
            MountCommand::Reauth(args) => {
                let receipt = args.run(output.clone()).await?;
                if output.is_structured() {
                    output.emit_result(ResultVerdict::Ok, receipt)?;
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
                let workspace = Workspace::resolve()?;
                let receipt = rm_with_options(&workspace, &name, output.yes(), dry_run, &output)?;
                if output.is_structured() {
                    output.emit_result(receipt.output_verdict(), &receipt)?;
                }
                Ok(ExitCode::Success)
            },
        }
    }
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct MountsResult {
    mounts: Vec<crate::inventory::MountStatus>,
    verdict: crate::inventory::Verdict,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct MountShowResult {
    mount: crate::inventory::MountStatus,
    frontends: Vec<crate::inventory::FrontendStatus>,
    access_paths: Vec<crate::inventory::AccessPath>,
    verdict: crate::inventory::Verdict,
    /// Local desired-state spec path. Absent when the mount is only observed
    /// through the daemon (no local spec, e.g. `observed_mount_rows`) or the
    /// local registry itself failed to parse.
    spec_path: Option<std::path::PathBuf>,
    auth_kind: Option<omnifs_workspace::authn::AuthKind>,
    /// Compact `key: value, ...` rendering of the provider config object, if
    /// the mount configures one.
    config_summary: Option<String>,
}

async fn ls(_args: &LsArgs, output: Output) -> anyhow::Result<ExitCode> {
    let workspace = Workspace::resolve()?;
    let result = list_with_output(&workspace).await?;
    let exit_code = match result.verdict {
        crate::inventory::Verdict::Ok => ExitCode::Success,
        crate::inventory::Verdict::Degraded => ExitCode::Degraded,
    };
    if output.is_structured() {
        output.emit_result(result.verdict, &result)?;
    } else {
        crate::ui::print_raw(&render_mounts(&result));
    }
    Ok(exit_code)
}

async fn show(args: &ShowArgs, output: Output) -> anyhow::Result<ExitCode> {
    let workspace = Workspace::resolve()?;
    let result = show_with_output(&workspace, &args.name).await?;
    if output.is_structured() {
        output.emit_result(result.verdict, &result)?;
    } else {
        crate::ui::print_raw(&render_mount_show(&result));
    }
    Ok(match result.verdict {
        crate::inventory::Verdict::Ok => ExitCode::Success,
        crate::inventory::Verdict::Degraded => ExitCode::Degraded,
    })
}

pub(crate) async fn list_with_output(workspace: &Workspace) -> anyhow::Result<MountsResult> {
    let inventory = crate::inventory::Inventory::collect(workspace).await?;
    let verdict = inventory.verdict();
    Ok(MountsResult {
        mounts: inventory.mounts,
        verdict,
    })
}

pub(crate) async fn show_with_output(
    workspace: &Workspace,
    name: &str,
) -> anyhow::Result<MountShowResult> {
    let inventory = crate::inventory::Inventory::collect(workspace).await?;
    let mount = inventory
        .mounts
        .iter()
        .find(|mount| mount.name == name)
        .cloned()
        .ok_or_else(|| anyhow!("no mount named `{name}`"))?;
    let mount_name = MountName::new(name.to_owned())?;
    let access_paths = inventory.access_paths(&mount_name);
    let verdict = inventory.verdict();
    // A best-effort local-spec lookup: an observed-but-not-locally-configured
    // mount, or a workspace whose registry has an unrelated parse failure,
    // still gets a card, just without these locally-sourced facts.
    let local = crate::mount_config::load_mounts(workspace)
        .ok()
        .and_then(|mounts| mounts.into_iter().find(|entry| entry.name == mount_name));
    let spec_path = local.as_ref().map(|entry| entry.source.clone());
    let auth_kind = local
        .as_ref()
        .and_then(|entry| entry.config.auth.as_ref())
        .map(omnifs_workspace::mounts::Auth::kind);
    let config_summary = local
        .as_ref()
        .and_then(|entry| entry.config.config_raw.as_ref())
        .and_then(config_summary_line);
    Ok(MountShowResult {
        mount,
        frontends: inventory.frontends,
        access_paths,
        verdict,
        spec_path,
        auth_kind,
        config_summary,
    })
}

/// `key: value, ...` for a provider config object, or `None` for an empty or
/// non-object config value.
fn config_summary_line(value: &serde_json::Value) -> Option<String> {
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

fn render_mounts(result: &MountsResult) -> String {
    let mut report = crate::ui::table::Report::new();
    report.push(crate::ui::table::Block::Resources(
        crate::status::mount_table(&result.mounts),
    ));
    report.render()
}

/// `mount show`'s detail card: a header line naming the mount with
/// its headline state right-aligned (the same precedence `mount ls` uses,
/// [`crate::status::mount_row_state`]), then an indented definition list of
/// the facts a maintainer actually reaches for: provider pin, auth, spec
/// path, access, and provider config. `mount ls` keeps owning the tabular
/// summary; this is deliberately a different shape, not a one-row table.
fn render_mount_show(result: &MountShowResult) -> String {
    use crate::ui::table::{Block, ContextStrip, Report};

    let state = crate::status::mount_row_state(&result.mount);
    let mut report = Report::new();
    report.push(Block::Context(ContextStrip::new(
        result.mount.name.clone(),
        String::new(),
        state,
    )));
    let mut card = report.render();

    card.push_str(&detail_rows(&detail_card_facts(result)));
    card.push('\n');
    card
}

/// The ordered `(key, value)` facts below a `mount show` card's header.
/// `auth` and `config` are omitted, not shown empty, when the mount has
/// neither. `access` always has at least one row: a fallback fact when nothing
/// currently reaches the mount, never a silently missing key.
fn detail_card_facts(result: &MountShowResult) -> Vec<(&'static str, String)> {
    let mut facts = vec![("provider", provider_fact(&result.mount.provider))];
    if let Some(fact) = auth_fact(result) {
        facts.push(("auth", fact));
    }
    if let Some(path) = &result.spec_path {
        facts.push(("spec", omnifs_workspace::display(path)));
    }
    let access_rows: Vec<String> = result
        .access_paths
        .iter()
        .filter(|path| {
            matches!(
                path.state,
                crate::inventory::AccessState::Available | crate::inventory::AccessState::Offline
            )
        })
        .map(crate::ui::access::access_row)
        .collect();
    if access_rows.is_empty() {
        facts.push(("access", "no frontend attached yet".to_owned()));
    } else {
        facts.extend(access_rows.into_iter().map(|row| ("access", row)));
    }
    if let Some(config) = &result.config_summary {
        facts.push(("config", config.clone()));
    }
    facts
}

/// `<name>@<version>  (pin <digest8>…)`, with the provider's own state
/// parenthesized only when it is not the healthy default (the clean
/// example never shows a healthy pin's state; a missing/corrupt pin still
/// must not go silent on this card).
fn provider_fact(pin: &crate::inventory::ProviderPin) -> String {
    use std::fmt::Write as _;

    let mut value = format!(
        "{}@{}",
        pin.name,
        pin.version.as_deref().unwrap_or("unpinned")
    );
    if !matches!(pin.state, crate::inventory::ProviderPinState::Available) {
        let _ = write!(value, " ({})", pin.state.label());
    }
    let short = &pin.artifact[..pin.artifact.len().min(8)];
    let _ = write!(value, "  (pin {short}…)");
    value
}

/// `<kind>, <state>` when a local spec resolved the auth kind (the
/// `oauth, signed in as raulk`, minus the upstream identity: neither the
/// OAuth flow nor the CLI-visible credential API surfaces one today, so this
/// card states the kind and state it can actually observe rather than
/// fabricating a username). `None` for a mount needing no auth at all, or one
/// whose local spec could not be resolved.
fn auth_fact(result: &MountShowResult) -> Option<String> {
    let kind = result.auth_kind?;
    Some(format!("{kind}, {}", result.mount.auth.label()))
}

/// One line per fact, two-space indented, no glyph column: a definition list
/// reads differently from a settled-operation ledger, so this deliberately
/// does not reuse `render.rs`'s glyph-led `LedgerRow` vocabulary.
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
        let workspace = Workspace::resolve()?;
        let prompt = PromptMode::from_flags(output.yes(), output.no_input());
        let result = self.run_with_output(&workspace, &output, prompt).await;
        if result.is_ok() {
            output.outro(format!("Re-authenticated `{}`.", self.name));
        }
        result?;
        Ok(crate::commands::receipt::MountReauthReceipt {
            verdict: crate::commands::receipt::Verdict::Ok,
            mount: self.name.clone(),
        })
    }

    pub(crate) async fn run_with_output(
        &self,
        workspace: &Workspace,
        output: &crate::ui::output::Output,
        prompt: PromptMode,
    ) -> anyhow::Result<()> {
        let mount_name = self.name.as_str();
        let mounts = crate::mount_config::load_mounts(workspace)?;
        let mount_config = mounts
            .iter()
            .find(|m| m.name.as_str() == mount_name)
            .ok_or_else(|| {
                anyhow!(
                    "no mount named `{mount_name}`; run `omnifs mount add <provider>` to create it"
                )
            })?;
        let Some(auth) = mount_config.config.auth.as_ref() else {
            anyhow::bail!("mount `{mount_name}` needs no authentication");
        };

        let provider = workspace
            .catalog()
            .get(&mount_config.config.provider.id)?
            .ok_or_else(|| {
                anyhow!(
                    "provider artifact `{}` for mount `{mount_name}` is missing",
                    mount_config.config.provider.id
                )
            })?;
        let manifest = provider.manifest()?;

        let selection = crate::auth::AuthSelection {
            auth_type: auth.kind(),
            scheme: auth.scheme().map(str::to_owned),
            account: auth.account().map(str::to_owned),
        };

        // `--no-input` must never reach an OAuth browser handoff (it would hang
        // on the browser confirm or the manual-code paste). Mirror the add-side
        // guard: bail naming the interactive and static-token alternatives.
        let interactive = prompt.interactive;
        if !interactive && selection.is_oauth() {
            return Err(anyhow!(
                "`omnifs mount reauth {mount_name}` cannot complete OAuth without a terminal; run it interactively, or use a static-token scheme with --token - or --token-env VAR"
            ))
            .with_exit_code(ExitCode::AuthRequired);
        }

        // `reauth`'s own auth-outcome block shares the same key set `mount
        // add` uses for its completed-auth rows (`oauth`/`signed in`/
        // `credential`), since both flows route through the same
        // `login`/`run_static_token_init` primitives.
        let auth_key_width = crate::auth::auth_receipt_key_width();
        let target = if selection.is_oauth() {
            output.note(format!("re-authenticating `{mount_name}` over OAuth"));
            let target = crate::auth::login_with_workspace(
                workspace,
                mount_name,
                selection.account.as_deref(),
                crate::auth::LoginInteractivity {
                    no_browser: self.no_browser,
                    no_input: prompt.no_input,
                    scopes: &self.scopes,
                },
                output,
                auth_key_width,
            )
            .await?;
            target
        } else {
            let source = TokenSource::resolve(
                self.token.as_deref(),
                self.token_env.as_deref(),
                interactive,
            )?;
            let token = source.read(output)?;
            run_static_token_init(
                &manifest,
                &selection,
                token,
                workspace.credentials(),
                !self.no_validate,
                output,
                auth_key_width,
            )
            .await?
        };
        print_stored_credential_rows(output, &target);
        crate::metrics::maybe_print_health_nudge(workspace, output.clone()).await;
        Ok(())
    }
}

/// `reauth`'s second, independent ledger block: the exact credential keys
/// just stored. Its key set is dynamic (one row per `target.keys()`) but
/// known before this block's first row prints, so it sizes itself rather than
/// reusing the auth-outcome block's width the caller already printed.
fn print_stored_credential_rows(output: &crate::ui::output::Output, target: &CredentialTarget) {
    let rows: Vec<String> = target
        .keys()
        .into_iter()
        .map(|key| format!("credential `{key}`"))
        .collect();
    let key_width =
        Output::ledger_block_width(&rows.iter().map(String::as_str).collect::<Vec<_>>());
    for key in &rows {
        output.ledger_row(
            &crate::ui::render::LedgerRow::new(
                crate::ui::style::Glyph::Done,
                key.clone(),
                "stored; takes effect on the next `omnifs up` or `omnifs apply`",
            ),
            key_width,
        );
    }
}

#[allow(dead_code)]
pub fn rm(workspace: &Workspace, name: &str, yes: bool) -> anyhow::Result<()> {
    rm_with_options(
        workspace,
        name,
        yes,
        false,
        &Output::new(crate::ui::output::OutputMode::Human, false),
    )
    .map(|_| ())
}

#[allow(clippy::too_many_lines)] // plan, decision, and receipt stay linear
fn rm_with_options(
    workspace: &Workspace,
    name: &str,
    yes: bool,
    dry_run: bool,
    output: &Output,
) -> anyhow::Result<crate::commands::receipt::MountRemoveReceipt> {
    let output = output.clone();
    let mounts = crate::mount_config::load_mounts(workspace)?;
    let name =
        MountName::new(name.to_owned()).with_context(|| format!("invalid mount name `{name}`"))?;

    let Some(mount) = mounts.iter().find(|m| m.name == name) else {
        // Removing an already-absent valid mount is an idempotent cleanup
        // operation. Emit the same plan/receipt shape as other destructive
        // commands, but never construct a credential service or touch the
        // credential store when there is no spec to remove.
        let mut plan = Plan::new(format!("Removing mount `{name}`"));
        plan.push(Row::keep(
            "spec",
            "spec",
            format!(
                "{} (already absent)",
                omnifs_workspace::display(&workspace.desired_state().spec_path(&name))
            ),
        ));
        output.plan(&plan);
        if let Some(suggestion) = mounts
            .iter()
            .map(|mount| mount.name.to_string())
            .find(|candidate| candidate.starts_with(name.as_str()))
        {
            output.note(format!("Did you mean `{suggestion}`?"));
        }
        if dry_run {
            output.outro("Dry run, nothing changed.");
            return Ok(crate::commands::receipt::MountRemoveReceipt::dry_run(
                name.to_string(),
                plan,
            ));
        }
        let receipt = plan.receipt([Outcome::skip("spec", "already absent")]);
        output.receipt(&receipt);
        output.outro(format!("Mount `{name}` already absent."));
        return Ok(crate::commands::receipt::MountRemoveReceipt::applied(
            name.to_string(),
            plan,
            receipt.rows,
        ));
    };
    let config_path = mount.source.clone();
    // Build the plan without constructing an HTTP client or registering an
    // OAuth revocation. A dry run must stop before any apply-only work.
    let plan = mount_remove_plan(&name, &config_path);
    output.plan(&plan);
    match Decision::resolve(
        PromptMode::from_flags(yes || output.yes(), output.no_input()),
        dry_run,
        "Remove?",
        "-y",
        &output,
    )? {
        Decision::DryRun => {
            output.outro("Dry run, nothing changed.");
            return Ok(crate::commands::receipt::MountRemoveReceipt::dry_run(
                name.to_string(),
                plan,
            ));
        },
        Decision::Apply => {},
    }

    let spec_outcome = match workspace.desired_state().remove_uncommitted(&name) {
        Ok(true) => Outcome::done("spec", "desired-state deletion recorded"),
        Ok(false) => Outcome::skip("spec", "already absent"),
        Err(error) => Outcome::fail("spec", format!("spec kept; local delete failed: {error:#}")),
    };
    let mut outcomes = vec![spec_outcome];
    if outcomes[0].state != crate::ui::consent::OutcomeState::Fail
        && let Err(error) = workspace.desired_state().commit()
    {
        outcomes[0] = Outcome::fail(
            "spec",
            format!("deleted locally; desired-state commit failed: {error:#}"),
        );
    }
    let receipt = plan.receipt(outcomes);
    output.receipt(&receipt);
    output.outro(format!("Removed `{name}`. {}", plan.settled_summary()));
    if receipt
        .rows
        .iter()
        .any(|row| row.id == "spec" && row.state == crate::ui::consent::OutcomeState::Fail)
    {
        anyhow::bail!(
            receipt
                .rows
                .iter()
                .find(|row| row.id == "spec")
                .map_or_else(
                    || "mount spec removal failed".to_owned(),
                    |row| row.value.clone()
                )
        );
    }
    Ok(crate::commands::receipt::MountRemoveReceipt::applied(
        name.to_string(),
        plan,
        receipt.rows,
    ))
}

fn mount_remove_plan(name: &MountName, config_path: &Path) -> Plan {
    let mut plan = Plan::new(format!("Removing mount `{name}`"));
    plan.push(Row::remove(
        "spec",
        "spec",
        omnifs_workspace::display(config_path).clone(),
    ));
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::fixture_workspace as base_fixture_workspace;
    use tempfile::TempDir;

    fn fixture_workspace(root: &Path) -> omnifs_workspace::Workspace {
        let workspace = base_fixture_workspace(root);
        std::fs::create_dir_all(root.join("mounts")).unwrap();
        workspace
    }

    #[tokio::test]
    async fn rejects_invalid_mount_name() {
        let tmp = TempDir::new().unwrap();
        let workspace = fixture_workspace(tmp.path());
        let err = rm(&workspace, "../leak", true).unwrap_err();
        assert!(format!("{err:#}").contains("invalid mount name"));
    }

    #[tokio::test]
    async fn removing_missing_valid_mount_is_a_noop_without_credentials() {
        let tmp = TempDir::new().unwrap();
        let workspace = fixture_workspace(tmp.path());
        rm(&workspace, "missing", true).unwrap();
        assert!(!tmp.path().join("credentials.json").exists());
    }

    #[test]
    fn removal_plan_names_desired_state_row() {
        let name = MountName::try_from("github").unwrap();
        let path = Path::new("/tmp/omnifs/mounts/github.json");
        let plan = mount_remove_plan(&name, path);
        assert_eq!(plan.remove_count(), 1);
        assert_eq!(plan.rows[0].id, "spec");
        assert_eq!(plan.title, "Removing mount `github`");
    }

    #[tokio::test]
    async fn removing_an_absent_mount_exits_zero_and_settles_a_skip_receipt() {
        let tmp = TempDir::new().unwrap();
        let workspace = fixture_workspace(tmp.path());
        let output = Output::new(crate::ui::output::OutputMode::Human, false);
        let receipt = rm_with_options(&workspace, "missing", true, false, &output).unwrap();
        assert_eq!(receipt.mount, "missing");
        assert!(
            receipt
                .rows
                .iter()
                .any(|row| row.id == "spec" && row.state == crate::ui::consent::OutcomeState::Skip)
        );
    }

    /// `--dry-run` prints the plan and settles nothing: the
    /// desired-state directory is untouched.
    #[tokio::test]
    async fn dry_run_prints_the_plan_and_removes_nothing() {
        let tmp = TempDir::new().unwrap();
        let workspace = fixture_workspace(tmp.path());
        AddArgs {
            provider: Some("dns".to_string()),
            as_name: None,
            no_browser: true,
            token: None,
            token_env: None,
            no_validate: false,
            scopes: Vec::new(),
            scheme: None,
            no_auth: false,
            config_json: None,
            limits_json: None,
        }
        .run_in_workspace(
            &workspace,
            Output::new(crate::ui::output::OutputMode::Human, false),
        )
        .await
        .unwrap();
        let spec_path = tmp.path().join("mounts/dns.json");
        assert!(spec_path.exists(), "fixture must create the spec first");

        let output = Output::new(crate::ui::output::OutputMode::Human, false);
        let receipt = rm_with_options(&workspace, "dns", true, true, &output).unwrap();
        assert!(spec_path.exists(), "dry run must not remove the spec file");
        assert_eq!(receipt.rows.len(), 0, "a dry run settles no receipt rows");
        assert!(receipt.dry_run);
    }

    /// `omnifs mount ls` renders exactly the Mounts section of the status
    /// report, never the context strip or
    /// the Frontends table alongside it.
    #[test]
    fn render_mounts_is_exactly_the_status_mounts_section() {
        let mounts = vec![crate::inventory::MountStatus {
            name: "github".into(),
            root: "/github".into(),
            provider: crate::inventory::ProviderPin {
                name: "github".into(),
                version: Some("0.3.2".into()),
                artifact: "a".repeat(64),
                state: crate::inventory::ProviderPinState::Available,
            },
            auth: crate::inventory::AuthState::Ready,
            serving: crate::inventory::ServingState::Live,
            access_count: 1,
            fix: None,
        }];
        let result = MountsResult {
            mounts: mounts.clone(),
            verdict: crate::inventory::Verdict::Ok,
        };
        let rendered = render_mounts(&result);
        assert!(rendered.contains("Mounts"));
        assert!(rendered.contains("github"));
        assert!(!rendered.contains("omnifs  "), "{rendered:?}");
        assert!(!rendered.contains("Frontends"), "{rendered:?}");

        let mut expected = crate::ui::table::Report::new();
        expected.push(crate::ui::table::Block::Resources(
            crate::status::mount_table(&mounts),
        ));
        assert_eq!(rendered, expected.render());
    }

    fn show_result(mount: crate::inventory::MountStatus) -> MountShowResult {
        MountShowResult {
            mount,
            frontends: Vec::new(),
            access_paths: Vec::new(),
            verdict: crate::inventory::Verdict::Ok,
            spec_path: Some("/home/.omnifs/mounts/github.json".into()),
            auth_kind: Some(omnifs_workspace::authn::AuthKind::OAuth),
            config_summary: Some(r#"org_filter: "raulk""#.to_owned()),
        }
    }

    fn healthy_mount() -> crate::inventory::MountStatus {
        crate::inventory::MountStatus {
            name: "github".into(),
            root: "/github".into(),
            provider: crate::inventory::ProviderPin {
                name: "github".into(),
                version: Some("0.3.2".into()),
                artifact: "a1b2c3d4".to_owned() + &"e".repeat(56),
                state: crate::inventory::ProviderPinState::Available,
            },
            auth: crate::inventory::AuthState::Ready,
            serving: crate::inventory::ServingState::Live,
            access_count: 1,
            fix: None,
        }
    }

    /// `mount show` is a detail card, never the tabular
    /// single-row table `render_mounts` (`mount ls`) already owns.
    #[test]
    fn render_mount_show_is_a_detail_card_not_a_table() {
        let rendered = render_mount_show(&show_result(healthy_mount()));
        let lines = rendered.lines().collect::<Vec<_>>();
        assert!(lines[0].starts_with("github"), "{rendered:?}");
        assert!(lines[0].trim_end().ends_with("live"), "{rendered:?}");
        assert!(!rendered.contains("Mounts"), "{rendered:?}");
        assert!(!rendered.contains("Access paths"), "{rendered:?}");

        let provider_line = lines
            .iter()
            .find(|line| line.trim_start().starts_with("provider"))
            .expect("provider row");
        assert!(provider_line.contains("github@0.3.2"), "{rendered:?}");
        assert!(provider_line.contains("(pin a1b2c3d4…)"), "{rendered:?}");

        let auth_line = lines
            .iter()
            .find(|line| line.trim_start().starts_with("auth"))
            .expect("auth row");
        assert!(auth_line.contains("oauth"), "{rendered:?}");
        assert!(auth_line.contains("ready"), "{rendered:?}");

        let spec_line = lines
            .iter()
            .find(|line| line.trim_start().starts_with("spec"))
            .expect("spec row");
        assert!(spec_line.contains("mounts/github.json"), "{rendered:?}");

        let config_line = lines
            .iter()
            .find(|line| line.trim_start().starts_with("config"))
            .expect("config row");
        assert!(
            config_line.contains(r#"org_filter: "raulk""#),
            "{rendered:?}"
        );
    }

    /// `access` states "no frontend attached yet" rather than silently
    /// omitting the row when nothing currently reaches the mount.
    #[test]
    fn detail_card_access_row_falls_back_without_a_reachable_frontend() {
        let mut result = show_result(healthy_mount());
        result.access_paths = Vec::new();
        let facts = detail_card_facts(&result);
        let access = facts
            .iter()
            .find(|(key, _)| *key == "access")
            .expect("access fact");
        assert_eq!(access.1, "no frontend attached yet");
    }

    /// `auth` and `config` are omitted, not shown empty, when the mount has
    /// neither (most providers have no config; a mount with no local spec has
    /// no locally-resolvable auth kind).
    #[test]
    fn detail_card_omits_auth_and_config_when_absent() {
        let mut result = show_result(healthy_mount());
        result.auth_kind = None;
        result.config_summary = None;
        let facts = detail_card_facts(&result);
        assert!(!facts.iter().any(|(key, _)| *key == "auth"), "{facts:?}");
        assert!(!facts.iter().any(|(key, _)| *key == "config"), "{facts:?}");
    }

    #[test]
    fn provider_fact_surfaces_a_degraded_pin_state() {
        let mut pin = healthy_mount().provider;
        pin.state = crate::inventory::ProviderPinState::Missing;
        assert!(provider_fact(&pin).contains("(missing)"));
    }
}
