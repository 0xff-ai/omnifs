//! `omnifs setup`: the guided first-run walkthrough. A thin
//! composition over `mount add`'s stages, `up`'s launch choreography, and
//! `frontend enable`, narrated as three numbered steps rather than as a
//! sequence of separate command invocations.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use clap::Args;

use crate::commands::frontend::{
    FrontendEnableArgs, FrontendFilesystem, FrontendResult, FrontendResultState, FrontendRuntime,
    available_frontends,
};
use crate::commands::mount::AddArgs;
use crate::commands::up::UpArgs;
use crate::error::ExitCode;
use crate::inventory::Inventory;
use crate::provider_bundle::EmbeddedProviders;
use crate::provider_resolver::{provider_options, safe_for_setup};
use crate::stages::{PromptMode, ReceiptStyle};
use crate::ui::live::LiveRegion;
use crate::ui::output::Output;
use crate::ui::render::{self, Capabilities};
use crate::ui::style::{self, Glyph};
use omnifs_workspace::Workspace;

#[derive(Args, Debug, Clone, Default)]
pub struct SetupArgs {
    /// Configure these exact embedded provider names. Repeat or comma-separate.
    #[arg(long, value_name = "PROVIDER", value_delimiter = ',')]
    pub providers: Vec<String>,
    /// Configure mounts without starting the daemon or enabling frontends.
    #[arg(long)]
    pub no_up: bool,
    /// Print OAuth URLs instead of opening a browser.
    #[arg(long)]
    pub no_browser: bool,
}

impl SetupArgs {
    pub async fn run(self, output: Output) -> Result<ExitCode> {
        let workspace = Workspace::resolve()?;
        self.run_in_workspace(&workspace, output).await
    }

    async fn run_in_workspace(self, workspace: &Workspace, output: Output) -> Result<ExitCode> {
        let started = Instant::now();
        let prompt =
            PromptMode::from_flags(output.yes(), output.no_input() || output.is_structured());
        let caps = crate::ui::output::stderr_capabilities(output.quiet());

        crate::ui::splash::show(caps, output.no_input(), output.is_structured())?;

        output.narrate("Welcome. Three steps: pick services, sign in, choose how files appear.");
        output.narrate("");
        output.heading("1. Services");
        let selected = self.select_providers(workspace, &output, prompt)?;
        let configure_prompt = Self::configure_prompt(&output, prompt);

        output.narrate("");
        output.heading("2. Sign in");
        for provider in selected {
            crate::stages::configure_mount(
                AddArgs {
                    provider: Some(provider),
                    as_name: None,
                    no_browser: self.no_browser,
                    token: None,
                    token_env: None,
                    no_validate: false,
                    scopes: Vec::new(),
                    scheme: None,
                    no_auth: false,
                    config_json: None,
                    limits_json: None,
                },
                workspace,
                &output,
                configure_prompt,
                ReceiptStyle::Compact,
            )
            .await?;
        }

        if !self.no_up && !crate::mount_config::load_mounts(workspace)?.is_empty() {
            output.narrate("");
            output.heading("3. Your files");
            output.narrate("A frontend serves the tree to your OS. You can enable more than one.");
            let frontends = select_frontends(&output, prompt)?;
            output.narrate("");

            UpArgs::default()
                .start_in_workspace(workspace, output.clone())
                .await?;

            if !frontends.is_empty() {
                Self::enable_frontends(workspace, &output, &frontends).await?;
            }
        }

        let inventory = Inventory::collect(workspace).await?;
        let exit_code = match inventory.verdict() {
            crate::inventory::Verdict::Ok => ExitCode::Success,
            crate::inventory::Verdict::Degraded => ExitCode::Degraded,
        };
        if output.is_structured() {
            output.emit_result(inventory.verdict(), inventory)?;
        } else {
            Self::print_closing_block(&inventory, &output, started, caps);
        }
        Ok(exit_code)
    }

    fn select_providers(
        &self,
        workspace: &Workspace,
        output: &Output,
        prompt: PromptMode,
    ) -> Result<Vec<String>> {
        let embedded = EmbeddedProviders::load()?;
        let mounts = crate::mount_config::load_mounts(workspace)?;
        let configured_names = mounts
            .iter()
            .map(|mount| mount.config.provider.meta.name.to_string())
            .collect::<BTreeSet<_>>();

        if !self.providers.is_empty() {
            let mut seen = BTreeSet::new();
            for provider in &self.providers {
                if seen.insert(provider) && embedded.by_name(provider).is_none() {
                    bail!(
                        "provider `{provider}` is not an exact embedded provider name; pass one of: {}",
                        embedded.names().collect::<Vec<_>>().join(", ")
                    );
                }
            }
            let (new, skipped) = split_requested_providers(&self.providers, &configured_names);
            // Every skip repeats the same literal key, so the block's width
            // is just that key's own width regardless of how many print.
            let key_width = Output::ledger_block_width(&["provider"]);
            for provider in &skipped {
                output.ledger_row(
                    &crate::ui::render::LedgerRow::new(
                        Glyph::Skip,
                        "provider",
                        format!("{provider} already configured"),
                    ),
                    key_width,
                );
            }
            return Ok(new);
        }

        // `provider_options` wants a name->mount-name map; the mount name is
        // never read by its filtering (only `contains_key` is), so an empty
        // placeholder value is fine.
        let configured_map: BTreeMap<String, String> = configured_names
            .iter()
            .map(|name| (name.clone(), String::new()))
            .collect();

        if output.yes() {
            let selected = provider_options(&embedded, &configured_map)
                .into_iter()
                .filter(|option| {
                    embedded
                        .by_name(&option.name)
                        .is_some_and(|provider| safe_for_setup(provider.manifest()))
                })
                .map(|option| option.name)
                .collect();
            return Ok(selected);
        }

        if prompt.no_input || !prompt.interactive {
            bail!("setup needs --providers <NAME>, or pass --yes to select safe providers");
        }

        let options = provider_options(&embedded, &configured_map);
        if options.is_empty() {
            return Ok(Vec::new());
        }
        let choices = options.into_iter().map(|option| {
            let detail = embedded
                .by_name(&option.name)
                .map(|entry| crate::capability::consent_detail(entry.manifest()))
                .unwrap_or_default();
            let checked = option.default_selected;
            (option.name.clone(), option.name, detail, checked)
        });
        crate::ui::prompt::MultiSelect::new("Which services should setup configure?", "services")
            .detailed_options(choices)
            .ask_with_output(output)
    }

    fn configure_prompt(output: &Output, prompt: PromptMode) -> PromptMode {
        if output.yes() {
            PromptMode {
                interactive: false,
                yes: true,
                no_input: true,
            }
        } else {
            prompt
        }
    }

    /// Enable every selected frontend, aggregating the outcome into one
    /// `frontends` ledger row
    /// instead of one row per runtime: the multi-select echo above already
    /// named which frontends setup chose, so the outcome only needs a count.
    /// Setup uses no reconnect-grace machinery because it runs these enables
    /// itself, so this reuses
    /// `FrontendEnableArgs::enable` directly rather than `Launcher`'s
    /// reattachment wait.
    ///
    /// Each `enable` call narrates its own progress (a Docker image pull
    /// spinner, container lifecycle lines) through `Output`, and left alone
    /// those rows would print into scrollback while the aggregate region
    /// above is still drawn, so every redraw of one fights the other's
    /// cursor math. Instead, each `enable` gets an `Output` clone whose
    /// narration is redirected (`Output::with_narration_sink`) into this
    /// region's one `frontends` line, combined with the running
    /// `n/m attaching…` counter, so a slow image pull still reads as live
    /// text instead of vanishing into a suppressed row. The region is
    /// shared behind a `Mutex` because the sink closure and this loop both
    /// need to drive it: the closure updates it live while `enable` runs,
    /// and the loop updates it between calls as each result lands.
    async fn enable_frontends(
        workspace: &Workspace,
        output: &Output,
        frontends: &[(FrontendFilesystem, FrontendRuntime)],
    ) -> Result<Vec<FrontendResult>> {
        let total = frontends.len();
        let key_width = Output::ledger_block_width(&["frontends"]);
        let region = Arc::new(Mutex::new(LiveRegion::new(output.clone(), ["frontends"])));
        update_region(&region, format!("0/{total} attaching…"));
        let mut results = Vec::with_capacity(total);
        for (filesystem, runtime) in frontends {
            let attached_so_far = results
                .iter()
                .filter(|result: &&FrontendResult| result.state == FrontendResultState::Attached)
                .count();
            let sink_region = Arc::clone(&region);
            let enable_output = output.clone().with_narration_sink(move |line| {
                update_region(
                    &sink_region,
                    format!("{attached_so_far}/{total} attaching… {line}"),
                );
            });
            let result = FrontendEnableArgs {
                filesystem: *filesystem,
                runtime: Some(*runtime),
                location: None,
            }
            .enable(workspace, enable_output)
            .await?;
            results.push(result);
            let attached = results
                .iter()
                .filter(|result| result.state == FrontendResultState::Attached)
                .count();
            update_region(&region, format!("{attached}/{total} attaching…"));
        }
        let attached = results
            .iter()
            .filter(|result| result.state == FrontendResultState::Attached)
            .count();
        let glyph = if attached == total {
            Glyph::Done
        } else {
            Glyph::Warn
        };
        // Every sink clone above is scoped to one loop iteration's `enable`
        // call and is dropped when that call returns, so by the time the
        // loop exits this is the only remaining handle: safe to reclaim the
        // region out of the `Arc<Mutex<_>>` and call the consuming
        // `finish`.
        let region = Arc::try_unwrap(region)
            .ok()
            .expect("no enable() call outlives its own loop iteration")
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        region.finish(
            glyph,
            "frontends",
            format!("{attached}/{total} attached"),
            key_width,
        );
        for result in &results {
            if let Some(fix) = &result.fix {
                output.note(fix);
            }
        }
        Ok(results)
    }

    /// The closing block: the tree reveal, the access lines,
    /// then the single closing sentence naming elapsed time and the
    /// suggested first command.
    fn print_closing_block(
        inventory: &Inventory,
        output: &Output,
        started: Instant,
        caps: Capabilities,
    ) {
        let tree = tree_lines(inventory, caps);
        let block = closing_block(inventory, tree, started.elapsed());
        for line in block.body {
            output.narrate(line);
        }
        output.outro(block.closing_sentence);
    }
}

/// Update the shared aggregate region from either the loop in
/// [`SetupArgs::enable_frontends`] or one of its per-enable narration sink
/// closures. A tiny free function rather than inlining the lock at each call
/// site, since both callers need the exact same poisoning behavior.
fn update_region(region: &Arc<Mutex<LiveRegion>>, text: String) {
    region
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .update("frontends", text);
}

/// Split `requested` provider names (the explicit `--providers` path) into
/// ones not yet configured and ones already present in `configured`,
/// preserving `requested`'s order. Pure so the skip-vs-select split is
/// testable without a real workspace: `configured` is exactly the set
/// `select_providers` derives from the existing mount registry.
fn split_requested_providers(
    requested: &[String],
    configured: &BTreeSet<String>,
) -> (Vec<String>, Vec<String>) {
    let mut new = Vec::new();
    let mut skipped = Vec::new();
    for provider in requested {
        if configured.contains(provider) {
            skipped.push(provider.clone());
        } else {
            new.push(provider.clone());
        }
    }
    (new, skipped)
}

/// Which frontends setup enables: every
/// frontend supported on this OS, pre-checked at the platform's recommended
/// default (`FrontendFilesystem::default_runtime`). `--yes`, `--no-input`,
/// and a non-interactive run all take that recommended default without
/// prompting, mirroring `PromptMode::resolve`'s explicit/yes/no-input
/// precedence even though a multi-select has no single "explicit" value to
/// check. A free function (not a `SetupArgs` method): it needs only the
/// invocation's output policy and prompt mode, never `SetupArgs` itself.
fn select_frontends(
    output: &Output,
    prompt: PromptMode,
) -> Result<Vec<(FrontendFilesystem, FrontendRuntime)>> {
    let available = available_frontends();
    if output.yes() || prompt.no_input || !prompt.interactive {
        return Ok(available
            .into_iter()
            .filter(|&(filesystem, runtime)| filesystem.default_runtime() == Some(runtime))
            .collect());
    }
    let choices = available.into_iter().map(|(filesystem, runtime)| {
        let checked = filesystem.default_runtime() == Some(runtime);
        (
            FrontendChoice {
                filesystem,
                runtime,
            },
            frontend_label(filesystem, runtime),
            vec![frontend_detail(filesystem, runtime).to_owned()],
            checked,
        )
    });
    let selected: Vec<FrontendChoice> = crate::ui::prompt::MultiSelect::new(
        "Which frontends should serve your files?",
        "frontends",
    )
    .detailed_options(choices)
    .ask_with_output(output)?;
    Ok(selected
        .into_iter()
        .map(|choice| (choice.filesystem, choice.runtime))
        .collect())
}

/// One frontend multi-select choice: a filesystem/runtime pair with a
/// `Display` impl (`MultiSelect<T>` requires one, the same way a plain
/// string value would satisfy it), so `select_frontends` can hand the
/// picker a real value type instead of a bare tuple.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FrontendChoice {
    filesystem: FrontendFilesystem,
    runtime: FrontendRuntime,
}

impl std::fmt::Display for FrontendChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&frontend_label(self.filesystem, self.runtime))
    }
}

/// The frontend multi-select's option label (`nfs (host)`).
fn frontend_label(filesystem: FrontendFilesystem, runtime: FrontendRuntime) -> String {
    format!("{filesystem} ({runtime})")
}

/// The frontend multi-select's detail-panel education copy: one
/// plain sentence naming what each filesystem/runtime combination actually
/// is, since a first-run user has no other way to know the difference
/// between, say, a libkrun and a Docker FUSE frontend.
fn frontend_detail(filesystem: FrontendFilesystem, runtime: FrontendRuntime) -> &'static str {
    match (filesystem, runtime) {
        (FrontendFilesystem::Nfs, FrontendRuntime::Host) => "Native mount, nothing to install.",
        (FrontendFilesystem::Fuse, FrontendRuntime::Host) => {
            "Native FUSE mount, nothing to install."
        },
        (FrontendFilesystem::Fuse, FrontendRuntime::Libkrun) => {
            "FUSE in a lightweight microVM, closest to Linux behavior."
        },
        (FrontendFilesystem::Fuse, FrontendRuntime::Docker) => {
            "FUSE in a container, for containerized workflows."
        },
        (FrontendFilesystem::Nfs, FrontendRuntime::Docker | FrontendRuntime::Libkrun) => {
            "NFS is host-only; this combination is not offered."
        },
    }
}

/// The tree-reveal root label: the attached host frontend's
/// location when one exists, else the guest wire mount point (display-only,
/// but still the right label when only a guest attach is live).
fn tree_root_label(inventory: &Inventory) -> String {
    crate::ui::access::primary_host_location(inventory).map_or_else(
        || crate::commands::frontend::GUEST_MOUNT.to_owned(),
        omnifs_workspace::display,
    )
}

/// One mount's tree-reveal annotation: the first few entry names at its
/// root, comma-joined, from a cheap bounded listing. `None` on any read
/// failure, an empty/dynamic root (a namespace like `dns/` that has nothing
/// to enumerate at its own root), or without a host frontend to list
/// through at all (a guest-only attach cannot be read from the host). Never
/// an error: every failure mode just omits the annotation.
fn mount_annotation(host_root: Option<&Path>, mount_name: &str) -> Option<String> {
    let root = host_root?;
    let entries = std::fs::read_dir(root.join(mount_name)).ok()?;
    let names: Vec<String> = entries
        .filter_map(std::result::Result::ok)
        .take(4)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    if names.is_empty() {
        return None;
    }
    Some(names.join(", "))
}

/// The tree-reveal lines for every mount in `inventory`, root
/// label plus per-mount listing via [`mount_annotation`].
fn tree_lines(inventory: &Inventory, caps: Capabilities) -> Vec<String> {
    let root = tree_root_label(inventory);
    let host_root = crate::ui::access::primary_host_location(inventory);
    let rows: Vec<(String, Option<String>)> = inventory
        .mounts
        .iter()
        .map(|mount| (mount.name.clone(), mount_annotation(host_root, &mount.name)))
        .collect();
    render_tree_lines(&root, &rows, caps)
}

/// The fixed gap between the longest mount name and its annotation, mirroring
/// `render.rs::LEDGER_GAP`'s role for ledger rows.
const TREE_GAP: usize = 4;

/// Pure tree-reveal render: the root
/// label, then one `├──`/`└──` row per mount with its annotation dim and
/// column-aligned to the longest mount name. Pure and split into lines (not
/// one joined block) so a fixed listing fixture can assert the exact shape,
/// and so the caller narrates each line the same way `up`'s access block
/// does.
fn render_tree_lines(
    root: &str,
    rows: &[(String, Option<String>)],
    caps: Capabilities,
) -> Vec<String> {
    let mut lines = vec![root.to_owned()];
    let name_width = rows
        .iter()
        .map(|(name, _)| render::display_width(name) + 1)
        .max()
        .unwrap_or(0);
    let last = rows.len().saturating_sub(1);
    for (index, (name, annotation)) in rows.iter().enumerate() {
        let connector = if index == last {
            "└── "
        } else {
            "├── "
        };
        let label = format!("{name}/");
        let mut line = format!("{connector}{label}");
        if let Some(annotation) = annotation {
            let pad = name_width.saturating_sub(render::display_width(&label)) + TREE_GAP;
            line.push_str(&" ".repeat(pad));
            line.push_str(&style::dim(annotation, caps.color));
        }
        lines.push(line);
    }
    lines
}

/// The closing block's ordered content: a blank line, the tree
/// reveal, a blank line, the access lines, then the closing sentence
/// (rendered separately through [`Output::outro`] so its wrapping and
/// "already closed" bookkeeping stay owned by `Output`).
struct ClosingBlock {
    body: Vec<String>,
    closing_sentence: String,
}

fn closing_block(inventory: &Inventory, tree: Vec<String>, elapsed: Duration) -> ClosingBlock {
    let mut body = vec![String::new()];
    body.extend(tree);
    body.push(String::new());
    body.extend(crate::ui::access::lines(inventory));
    ClosingBlock {
        body,
        closing_sentence: format!(
            "All set in {}. Browse:  `{}`",
            format_elapsed(elapsed),
            crate::ui::access::browse_command(inventory)
        ),
    }
}

/// `38s` under a minute, `2m 10s` at or above one.
fn format_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m {}s", secs / 60, secs % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::{
        AuthState, DaemonState, FrontendState, MountStatus, ProviderPin, ProviderPinState,
        ServingState,
    };
    use std::path::PathBuf;

    fn caps() -> Capabilities {
        Capabilities {
            width: 120,
            is_tty: false,
            color: false,
            quiet: false,
        }
    }

    // -- select_providers: --providers skip-vs-select split -----------------

    #[test]
    fn split_requested_providers_separates_new_from_already_configured() {
        let requested = vec!["github".to_owned(), "dns".to_owned(), "linear".to_owned()];
        let mut configured = BTreeSet::new();
        configured.insert("dns".to_owned());
        let (new, skipped) = split_requested_providers(&requested, &configured);
        assert_eq!(new, vec!["github".to_owned(), "linear".to_owned()]);
        assert_eq!(skipped, vec!["dns".to_owned()]);
    }

    #[test]
    fn split_requested_providers_is_all_new_when_nothing_is_configured() {
        let requested = vec!["github".to_owned()];
        let (new, skipped) = split_requested_providers(&requested, &BTreeSet::new());
        assert_eq!(new, requested);
        assert!(skipped.is_empty());
    }

    // -- select_frontends: --yes/--no-input/non-interactive all default -----

    fn prompt(interactive: bool, yes: bool, no_input: bool) -> PromptMode {
        PromptMode {
            interactive,
            yes,
            no_input,
        }
    }

    fn expected_default_frontends() -> Vec<(FrontendFilesystem, FrontendRuntime)> {
        available_frontends()
            .into_iter()
            .filter(|&(filesystem, runtime)| filesystem.default_runtime() == Some(runtime))
            .collect()
    }

    #[test]
    fn select_frontends_under_yes_takes_the_platform_default_without_prompting() {
        let output = Output::new(crate::ui::output::OutputMode::Human, false).with_yes(true);
        let selected = select_frontends(&output, prompt(true, true, false)).unwrap();
        assert_eq!(selected, expected_default_frontends());
    }

    #[test]
    fn select_frontends_under_no_input_takes_the_platform_default_without_prompting() {
        let output = Output::new(crate::ui::output::OutputMode::Human, false);
        let selected = select_frontends(&output, prompt(true, false, true)).unwrap();
        assert_eq!(selected, expected_default_frontends());
    }

    #[test]
    fn select_frontends_non_interactive_takes_the_platform_default_without_prompting() {
        let output = Output::new(crate::ui::output::OutputMode::Human, false);
        let selected = select_frontends(&output, prompt(false, false, false)).unwrap();
        assert_eq!(selected, expected_default_frontends());
    }

    #[test]
    fn frontend_label_and_detail_cover_every_available_combination() {
        for (filesystem, runtime) in available_frontends() {
            assert_eq!(
                frontend_label(filesystem, runtime),
                format!("{filesystem} ({runtime})")
            );
            assert!(!frontend_detail(filesystem, runtime).is_empty());
        }
    }

    // -- tree reveal: pure render from a fixed fixture -----------------------

    #[test]
    fn render_tree_lines_matches_the_documented_shape() {
        let rows = vec![
            ("github".to_owned(), Some("raulk, ethereum".to_owned())),
            ("dns".to_owned(), None),
        ];
        let lines = render_tree_lines("~/omnifs", &rows, caps());
        assert_eq!(lines[0], "~/omnifs");
        assert!(lines[1].starts_with("├── github/"), "{:?}", lines[1]);
        assert!(lines[1].contains("raulk, ethereum"), "{:?}", lines[1]);
        assert!(lines[2].starts_with("└── dns/"), "{:?}", lines[2]);
        // No annotation: the line ends right after the trailing slash.
        assert_eq!(lines[2], "└── dns/");
    }

    #[test]
    fn render_tree_lines_aligns_annotations_to_the_longest_mount_name() {
        let rows = vec![
            ("a".to_owned(), Some("x".to_owned())),
            ("much-longer-name".to_owned(), Some("y".to_owned())),
        ];
        let lines = render_tree_lines("~/omnifs", &rows, caps());
        let first_annotation_column = lines[1].find('x').unwrap();
        let second_annotation_column = lines[2].find('y').unwrap();
        assert_eq!(first_annotation_column, second_annotation_column);
    }

    #[test]
    fn mount_annotation_lists_the_first_entries_and_omits_on_empty_or_missing_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("github").join("raulk")).unwrap();
        std::fs::create_dir_all(root.join("github").join("ethereum")).unwrap();
        std::fs::create_dir_all(root.join("dns")).unwrap();

        let annotation = mount_annotation(Some(root), "github").unwrap();
        let mut names: Vec<&str> = annotation.split(", ").collect();
        names.sort_unstable();
        assert_eq!(names, vec!["ethereum", "raulk"]);

        assert!(mount_annotation(Some(root), "dns").is_none());
        assert!(mount_annotation(Some(root), "does-not-exist").is_none());
        assert!(mount_annotation(None, "github").is_none());
    }

    // -- closing block ordering ----------------------------------------------

    fn mount(name: &str) -> MountStatus {
        MountStatus {
            name: name.to_owned(),
            root: PathBuf::from(format!("/{name}")),
            provider: ProviderPin {
                name: name.to_owned(),
                version: None,
                artifact: "a".repeat(64),
                state: ProviderPinState::Available,
            },
            auth: AuthState::NotNeeded,
            serving: ServingState::Live,
            access_count: 1,
            fix: None,
        }
    }

    fn host_frontend() -> crate::inventory::FrontendStatus {
        crate::inventory::FrontendStatus {
            filesystem: FrontendFilesystem::Nfs,
            runtime: FrontendRuntime::Host,
            location: Some(PathBuf::from("/Users/raulk/omnifs")),
            state: FrontendState::Attached,
            scope: "all",
            mount_count: 1,
            fix: None,
        }
    }

    #[test]
    fn closing_block_orders_tree_then_access_lines_then_the_closing_sentence() {
        let inventory = Inventory::test(
            DaemonState::Running,
            vec![host_frontend()],
            vec![mount("github")],
        );
        let tree = vec!["~/omnifs".to_owned(), "└── github/".to_owned()];
        let block = closing_block(&inventory, tree.clone(), Duration::from_secs(38));
        let mut expected_body = vec![String::new()];
        expected_body.extend(tree);
        expected_body.push(String::new());
        expected_body.extend(crate::ui::access::lines(&inventory));
        assert_eq!(block.body, expected_body);
        assert_eq!(
            block.closing_sentence,
            format!(
                "All set in 38s. Browse:  `{}`",
                crate::ui::access::browse_command(&inventory)
            )
        );
    }

    #[test]
    fn format_elapsed_switches_units_at_one_minute() {
        assert_eq!(format_elapsed(Duration::from_secs(38)), "38s");
        assert_eq!(format_elapsed(Duration::from_secs(59)), "59s");
        assert_eq!(format_elapsed(Duration::from_mins(1)), "1m 0s");
        assert_eq!(format_elapsed(Duration::from_secs(130)), "2m 10s");
    }
}
