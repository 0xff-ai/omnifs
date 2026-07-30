//! `omnifs setup`: boot-and-orient.
//!
//! Starts the daemon, shows what is already running, lists every embedded
//! provider with an honestly derived auth label, then offers two quick-start
//! confirms: mount every provider that needs no sign-in in one atomic batch,
//! and attach the platform's recommended filesystem. Setup never selects
//! providers on the caller's behalf, never starts an OAuth flow, and never
//! handles credential material; anything that needs a sign-in or a config
//! value is left for `omnifs mount add`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use clap::Args;
use omnifs_api::ProviderMetadata;
use omnifs_core::fs;
use omnifs_provider::ProviderManifest;

use crate::client_fs_state::ClientFilesystemState;
use crate::client_state::ClientState;
use crate::commands::daemon_start;
use crate::error::ExitCode;
use crate::inventory::Inventory;
use crate::mutation::PlannedOp;
use crate::provider_catalog::{
    align_provider_catalog_rows, needs_no_sign_in, provider_catalog_row,
};
use crate::provider_resolver::ProviderResolver;
use crate::rpc::RpcClient;
use crate::ui::access::mount_next_action_line;
use crate::ui::output::{Output, PromptMode};
use crate::ui::render::{self, Capabilities, LedgerRow};
use crate::ui::style::{self, Glyph};

#[derive(Args, Debug, Clone, Default)]
#[command(after_help = "Examples:\n  omnifs setup")]
pub struct SetupArgs {}

impl SetupArgs {
    pub async fn run(self, output: Output) -> Result<ExitCode> {
        Box::pin(self.run_in_workspace(output)).await
    }

    async fn run_in_workspace(self, output: Output) -> Result<ExitCode> {
        daemon_start::start(&output).await?;
        let rpc = RpcClient::resolve()?;
        let state = ClientState::resolve()?;
        let started = Instant::now();
        let prompt = output.prompt_mode();
        let caps = crate::ui::output::stderr_capabilities(output.quiet());

        crate::ui::splash::show(caps, output.no_input(), output.is_structured())?;

        let daemon_inventory = rpc.inventory().await?;
        print_status_lines(&daemon_inventory, &output);

        let mounted_providers: BTreeSet<String> = daemon_inventory
            .mounts
            .iter()
            .map(|mount| mount.provider.name.clone())
            .collect();
        let embedded = rpc.list_embedded_providers().await?;
        let entries = catalog_entries(&embedded, &mounted_providers);
        print_provider_catalog(&entries, &output, caps);

        let mount_offer: Vec<String> = no_sign_in_offer(&entries)
            .into_iter()
            .map(|manifest| manifest.id.clone())
            .collect();
        let mounted = offer_quick_start_mounts(
            &rpc,
            &state,
            &output,
            prompt,
            &mount_offer,
            &embedded,
            &daemon_inventory.mounts,
        )
        .await?;

        let recommended = crate::commands::fs::recommended_filesystems();
        let fs_offer = filesystem_offer(&recommended, &daemon_inventory.attachments);
        let attached_host_location =
            offer_quick_start_filesystems(&output, prompt, &fs_offer).await?;

        print_next_block(&entries, &mounted, &output);

        let recommended_fs_id = crate::commands::fs::recommended_filesystem_id()?;
        let closing = closing_sentence(
            &mounted,
            attached_host_location.as_deref(),
            recommended_fs_id.as_ref(),
            started.elapsed(),
        );
        output.narrate("");
        output.outro(closing);

        let inventory = Inventory::collect_rpc().await?;
        let exit_code = match inventory.verdict() {
            crate::ui::output::ResultVerdict::Ok => ExitCode::Success,
            crate::ui::output::ResultVerdict::Degraded => ExitCode::Degraded,
        };
        if output.is_structured() {
            output.emit_result(inventory.verdict(), inventory)?;
        }
        Ok(exit_code)
    }
}

// -- status lines -------------------------------------------------------

fn print_status_lines(daemon_inventory: &omnifs_api::DaemonInventory, output: &Output) {
    let key_width = Output::ledger_block_width(&["daemon", "state"]);
    output.ledger_row(
        &LedgerRow::new(
            Glyph::Done,
            "daemon",
            format!("running (pid {})", daemon_inventory.info.pid),
        ),
        key_width,
    );
    output.ledger_row(
        &LedgerRow::new(
            Glyph::Done,
            "state",
            format!(
                "{}, {}",
                render::count(daemon_inventory.mounts.len(), "mount"),
                render::count(daemon_inventory.credentials.len(), "credential"),
            ),
        ),
        key_width,
    );
}

// -- provider catalog -----------------------------------------------------

/// One embedded provider as it appears in the catalog: its manifest, plus
/// whether it is already configured as a mount.
struct CatalogEntry {
    manifest: ProviderManifest,
    mounted: bool,
}

/// Every embedded provider that parses, alphabetized by name so the catalog
/// reads the same across runs regardless of the daemon's own listing order.
fn catalog_entries(embedded: &[ProviderMetadata], mounted: &BTreeSet<String>) -> Vec<CatalogEntry> {
    let mut entries: Vec<CatalogEntry> = embedded
        .iter()
        .filter_map(|entry| {
            let manifest = ProviderManifest::from_bytes(&entry.manifest).ok()?;
            let mounted = mounted.contains(&manifest.id);
            Some(CatalogEntry { manifest, mounted })
        })
        .collect();
    entries.sort_by(|left, right| left.manifest.id.cmp(&right.manifest.id));
    entries
}

/// The fixed gap after the widest command column in the "Next:" block,
/// mirroring `render.rs::LEDGER_GAP`'s role for ledger rows (the provider
/// catalog's own column gap lives with its alignment logic in
/// `provider_catalog.rs`).
const NEXT_GAP: usize = 4;

/// The provider catalog's printed rows: `name`, `description`, and
/// `auth label`, column-aligned via the same logic `mount add`'s interactive
/// picker uses, with a dim `mounted` marker appended to any row for a
/// provider already configured.
fn catalog_lines(entries: &[CatalogEntry], caps: Capabilities) -> Vec<String> {
    let rows: Vec<_> = entries
        .iter()
        .map(|entry| provider_catalog_row(&entry.manifest))
        .collect();
    entries
        .iter()
        .zip(align_provider_catalog_rows(&rows))
        .map(|(entry, mut line)| {
            if entry.mounted {
                line.push_str("  ");
                line.push_str(&style::dim("mounted", caps.color));
            }
            line
        })
        .collect()
}

fn print_provider_catalog(entries: &[CatalogEntry], output: &Output, caps: Capabilities) {
    if entries.is_empty() {
        return;
    }
    output.narrate("");
    output.heading("Providers you can mount:");
    output.narrate("");
    for line in catalog_lines(entries, caps) {
        output.narrate(line);
    }
}

/// The providers offered by the no-sign-in quick-start batch: every catalog
/// entry that needs no sign-in and is not already mounted.
fn no_sign_in_offer(entries: &[CatalogEntry]) -> Vec<&ProviderManifest> {
    entries
        .iter()
        .filter(|entry| !entry.mounted && needs_no_sign_in(&entry.manifest))
        .map(|entry| &entry.manifest)
        .collect()
}

fn mount_offer_question(offer: &[String]) -> String {
    format!(
        "Mount the {} that need no sign-in ({})?",
        render::count(offer.len(), "service"),
        offer.join(", ")
    )
}

/// Mount every provider in `offer` in one atomic batch if the operator
/// accepts, printing one `mounted` ledger row naming what got mounted.
/// Returns the mount names actually created (empty when the offer was empty,
/// declined, or answered no).
async fn offer_quick_start_mounts(
    rpc: &RpcClient,
    state: &ClientState,
    output: &Output,
    prompt: PromptMode,
    offer: &[String],
    embedded: &[ProviderMetadata],
    mounts: &[omnifs_api::MountRecord],
) -> Result<Vec<String>> {
    if offer.is_empty() {
        return Ok(Vec::new());
    }
    output.narrate("");
    if !resolve_offer_decision(output, prompt, mount_offer_question(offer))? {
        return Ok(Vec::new());
    }
    let mut ops = Vec::with_capacity(offer.len());
    let mut mounted = Vec::with_capacity(offer.len());
    for name in offer {
        let resolved = ProviderResolver::new(rpc).resolve(name, embedded).await?;
        let definition = crate::commands::mount::create::quick_start_definition(
            output,
            resolved.reference.id,
            &resolved.manifest,
            mounts,
        )?;
        mounted.push(definition.name.to_string());
        ops.push(PlannedOp::mount_create(definition));
    }
    let outcome = crate::mutation::run(rpc, state, output, || async move { Ok(ops) })
        .await?
        .context("quick-start mount batch produced no result")?;
    crate::mutation::narrate_serving(output, &outcome.serving);
    output.ledger_row(
        &LedgerRow::new(Glyph::Done, "mounted", mounted.join(", ")),
        Output::ledger_block_width(&["mounted"]),
    );
    Ok(mounted)
}

// -- filesystem quick-start -----------------------------------------------

/// `recommended`, minus whatever is already attached per the daemon's live
/// inventory.
fn filesystem_offer(
    recommended: &[(fs::Protocol, fs::Runtime)],
    attached: &[fs::Spec],
) -> Vec<(fs::Protocol, fs::Runtime)> {
    recommended
        .iter()
        .copied()
        .filter(|&(protocol, runtime)| {
            !attached
                .iter()
                .any(|spec| spec.protocol() == protocol && spec.runtime() == runtime)
        })
        .collect()
}

/// A short, friendly platform name for the filesystem quick-start question.
fn os_label() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macOS",
        "linux" => "Linux",
        other => other,
    }
}

fn filesystem_offer_question(
    offer: &[(fs::Protocol, fs::Runtime)],
    locations: &[String],
) -> String {
    let noun = if offer.len() == 1 {
        "filesystem"
    } else {
        "filesystems"
    };
    let joined = offer
        .iter()
        .zip(locations)
        .map(|(&(protocol, _), location)| format!("{protocol} at {location}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Attach the recommended {noun} for {} ({joined})?",
        os_label()
    )
}

/// Attach every filesystem in `offer` sequentially if the operator accepts,
/// with plain narration (the defaults are host-native and fast, so no
/// aggregate live region is needed the way a Docker image pull would want
/// one). Prints one `attached` ledger row naming what got attached. Returns
/// the first attached host-runtime filesystem's location, if any: the
/// closing sentence's browse hint needs a real host path to join a mount
/// name onto.
async fn offer_quick_start_filesystems(
    output: &Output,
    prompt: PromptMode,
    offer: &[(fs::Protocol, fs::Runtime)],
) -> Result<Option<PathBuf>> {
    if offer.is_empty() {
        return Ok(None);
    }
    let client_state = ClientFilesystemState::resolve()?;
    let mut locations = Vec::with_capacity(offer.len());
    for &(protocol, runtime) in offer {
        let location =
            crate::commands::fs::preview_filesystem_location(&client_state, protocol, runtime)?;
        locations.push(location.display().to_string());
    }
    output.narrate("");
    if !resolve_offer_decision(output, prompt, filesystem_offer_question(offer, &locations))? {
        return Ok(None);
    }
    let mut labels = Vec::with_capacity(offer.len());
    let mut host_location = None;
    for &(protocol, runtime) in offer {
        crate::commands::fs::ensure_setup_filesystem(protocol, runtime, output.clone()).await?;
        let location =
            crate::commands::fs::preview_filesystem_location(&client_state, protocol, runtime)?;
        let id = fs::Id::new(format!("{protocol}-{runtime}"))?;
        if runtime == fs::Runtime::Host {
            host_location.get_or_insert_with(|| location.clone());
        }
        labels.push(format!("{id} at {}", location.display()));
    }
    output.ledger_row(
        &LedgerRow::new(Glyph::Done, "attached", labels.join(", ")),
        Output::ledger_block_width(&["attached"]),
    );
    Ok(host_location)
}

/// The shared decision for both quick-start prompts: `--yes` always accepts;
/// `--no-input` and a non-interactive run both decline without prompting
/// (setup still exits successfully either way); otherwise ask, defaulting to
/// yes.
fn resolve_offer_decision(
    output: &Output,
    prompt: PromptMode,
    question: impl Into<String>,
) -> Result<bool> {
    crate::ui::consent::resolve_confirm(prompt, question, true, false, output)
}

// -- next block and closing ------------------------------------------------

fn next_block_lines(example_provider: Option<&str>) -> Vec<String> {
    let commands = [
        example_provider.map_or_else(
            || "omnifs mount add".to_owned(),
            |name| format!("omnifs mount add {name}"),
        ),
        "omnifs status".to_owned(),
        "omnifs fs ls".to_owned(),
    ];
    let descriptions = [
        "mount a service (opens sign-in if needed)",
        "see everything at a glance",
        "manage filesystems (fuse, microVM, docker)",
    ];
    let width = commands
        .iter()
        .map(|command| render::display_width(command))
        .max()
        .unwrap_or(0);
    commands
        .iter()
        .zip(descriptions)
        .map(|(command, description)| {
            let pad = width.saturating_sub(render::display_width(command)) + NEXT_GAP;
            format!("{command}{}{description}", " ".repeat(pad))
        })
        .collect()
}

fn print_next_block(entries: &[CatalogEntry], mounted: &[String], output: &Output) {
    output.narrate("");
    output.heading("Next:");
    output.narrate("");
    let example = entries
        .iter()
        .find(|entry| !entry.mounted && !mounted.iter().any(|name| name == &entry.manifest.id))
        .map(|entry| entry.manifest.id.as_str());
    for line in next_block_lines(example) {
        output.narrate(line);
    }
}

/// The closing sentence's four cases, in the order they are checked:
/// providers mounted and a host filesystem attached names both in a browse
/// hint; a filesystem attached with nothing newly mounted points at
/// `mount add`; providers mounted without an attached host filesystem points
/// at attaching the platform's recommended one; otherwise the sentence
/// stays plain. Pure so every case is testable without a live daemon.
fn closing_sentence(
    mounted: &[String],
    attached_host_location: Option<&Path>,
    recommended_fs_id: Option<&fs::Id>,
    elapsed: Duration,
) -> String {
    let elapsed = format_elapsed(elapsed);
    // The browse-or-attach hint is the same fact `mount add`'s adaptive
    // outro derives; `None` here only means "nothing was mounted this run",
    // since setup (unlike `mount add`) can genuinely have nothing to say yet.
    if let Some(action) = mount_next_action_line(
        mounted.first().map(String::as_str),
        attached_host_location,
        recommended_fs_id,
    ) {
        return format!("All set in {elapsed}. {action}");
    }
    if attached_host_location.is_some() {
        return format!("All set in {elapsed}. Add a service:  `omnifs mount add`");
    }
    format!("All set in {elapsed}.")
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
    use crate::provider_catalog::provider_auth_label;
    use omnifs_auth::{AuthScheme, OAuthFlow, OauthScheme, PkceLoopbackConfig, StaticTokenScheme};
    use omnifs_provider::{
        ConfigField, ConfigMetadata, ConfigType, HostResourceBinding, LimitDeclarations,
        PreopenMode, ProviderAuthManifest,
    };
    use std::collections::BTreeMap;

    fn caps() -> Capabilities {
        Capabilities {
            width: 120,
            is_tty: false,
            color: false,
            quiet: false,
        }
    }

    fn manifest(
        id: &str,
        auth: Option<ProviderAuthManifest>,
        config: Option<ConfigMetadata>,
    ) -> ProviderManifest {
        ProviderManifest {
            id: id.to_owned(),
            display_name: format!("{id} display"),
            description: Some(format!("{id} description")),
            provider: format!("{id}.wasm"),
            default_mount: id.to_owned(),
            version: None,
            wit_package: None,
            sdk_version: None,
            refresh_interval_secs: 0,
            capabilities: Vec::new(),
            limits: LimitDeclarations::default(),
            auth,
            config,
        }
    }

    fn oauth_auth() -> ProviderAuthManifest {
        ProviderAuthManifest {
            default: "oauth".to_owned(),
            guidance: BTreeMap::new(),
            schemes: vec![AuthScheme::Oauth(OauthScheme {
                key: "oauth".to_owned(),
                display_name: "OAuth".to_owned(),
                authorization_endpoint: "https://example.com/authorize".to_owned(),
                token_endpoint: "https://example.com/token".to_owned(),
                revocation_endpoint: None,
                default_client_id: Some("client".to_owned()),
                default_scopes: Vec::new(),
                flow: OAuthFlow::PkceLoopback(PkceLoopbackConfig {
                    redirect_uri_template: "http://127.0.0.1:{port}/cb".to_owned(),
                }),
                token_endpoint_auth: omnifs_auth::TokenEndpointAuthMethod::None,
                refresh_token_rotates: false,
                extra_authorize_params: Vec::new(),
                extra_token_params: Vec::new(),
                inject_domains: vec!["example.com".to_owned()],
                inject_header_name: None,
                inject_value_prefix: String::new(),
            })],
        }
    }

    fn static_token_auth() -> ProviderAuthManifest {
        ProviderAuthManifest {
            default: "pat".to_owned(),
            guidance: BTreeMap::new(),
            schemes: vec![AuthScheme::StaticToken(StaticTokenScheme {
                key: "pat".to_owned(),
                header_name: Some("Authorization".to_owned()),
                value_prefix: String::new(),
                description: "token".to_owned(),
                inject_domains: vec!["example.com".to_owned()],
                creation_url: None,
                validation: None,
                ambient_sources: Vec::new(),
            })],
        }
    }

    fn host_file_config() -> ConfigMetadata {
        ConfigMetadata {
            fields: vec![ConfigField {
                name: "path".to_owned(),
                value_type: ConfigType::String,
                required: false,
                default: None,
                description: None,
                binding: Some(HostResourceBinding::File {
                    mode: PreopenMode::Ro,
                }),
            }],
        }
    }

    // -- needs_no_sign_in / provider_auth_label ------------------------------

    #[test]
    fn needs_no_sign_in_requires_no_auth_and_no_mount_input() {
        assert!(needs_no_sign_in(&manifest("dns", None, None)));
        assert!(!needs_no_sign_in(&manifest(
            "github",
            Some(oauth_auth()),
            None
        )));
        assert!(!needs_no_sign_in(&manifest(
            "db",
            None,
            Some(host_file_config())
        )));
    }

    #[test]
    fn provider_auth_label_derives_from_the_manifest_alone() {
        assert_eq!(
            provider_auth_label(&manifest("dns", None, None)),
            "no sign-in"
        );
        assert_eq!(
            provider_auth_label(&manifest("db", None, Some(host_file_config()))),
            "needs config"
        );
        // `requires_mount_input` outranks an auth label even when the
        // provider also declares auth.
        assert_eq!(
            provider_auth_label(&manifest(
                "db",
                Some(oauth_auth()),
                Some(host_file_config())
            )),
            "needs config"
        );
        assert_eq!(
            provider_auth_label(&manifest("github", Some(oauth_auth()), None)),
            "needs sign-in"
        );
        assert_eq!(
            provider_auth_label(&manifest("linear", Some(static_token_auth()), None)),
            "needs a token"
        );
    }

    // -- catalog entries / lines ----------------------------------------------

    #[test]
    fn catalog_entries_marks_already_mounted_providers_and_sorts_by_name() {
        let embedded = vec![
            embedded_provider(&manifest("web", None, None)),
            embedded_provider(&manifest("arxiv", None, None)),
        ];
        let mut mounted = BTreeSet::new();
        mounted.insert("web".to_owned());
        let entries = catalog_entries(&embedded, &mounted);
        assert_eq!(entries[0].manifest.id, "arxiv");
        assert!(!entries[0].mounted);
        assert_eq!(entries[1].manifest.id, "web");
        assert!(entries[1].mounted);
    }

    #[test]
    fn catalog_lines_column_aligns_and_appends_a_mounted_marker() {
        let entries = vec![
            CatalogEntry {
                manifest: manifest("dns", None, None),
                mounted: false,
            },
            CatalogEntry {
                manifest: manifest("much-longer-name", Some(oauth_auth()), None),
                mounted: true,
            },
        ];
        let lines = catalog_lines(&entries, caps());
        let first_label_column = lines[0].find("no sign-in").unwrap();
        let second_label_column = lines[1].find("needs sign-in").unwrap();
        assert_eq!(first_label_column, second_label_column);
        assert!(lines[1].ends_with("mounted"), "{:?}", lines[1]);
        assert!(!lines[0].contains("mounted"), "{:?}", lines[0]);
    }

    #[test]
    fn no_sign_in_offer_excludes_mounted_and_sign_in_needed_providers() {
        let entries = vec![
            CatalogEntry {
                manifest: manifest("dns", None, None),
                mounted: false,
            },
            CatalogEntry {
                manifest: manifest("arxiv", None, None),
                mounted: true,
            },
            CatalogEntry {
                manifest: manifest("github", Some(oauth_auth()), None),
                mounted: false,
            },
        ];
        let offer: Vec<&str> = no_sign_in_offer(&entries)
            .into_iter()
            .map(|manifest| manifest.id.as_str())
            .collect();
        assert_eq!(offer, vec!["dns"]);
    }

    #[test]
    fn mount_offer_question_names_the_count_and_the_services() {
        let offer = vec!["dns".to_owned(), "arxiv".to_owned(), "web".to_owned()];
        assert_eq!(
            mount_offer_question(&offer),
            "Mount the 3 services that need no sign-in (dns, arxiv, web)?"
        );
    }

    fn embedded_provider(manifest: &ProviderManifest) -> ProviderMetadata {
        let bytes = serde_json::to_vec(manifest).unwrap();
        ProviderMetadata {
            reference: omnifs_api::ProviderReference {
                id: omnifs_core::ProviderId::from_wasm_bytes(bytes.as_slice()),
                name: manifest.id.clone(),
                version: None,
            },
            manifest: bytes,
        }
    }

    // -- filesystem offer ------------------------------------------------------

    fn spec(protocol: fs::Protocol, runtime: fs::Runtime) -> fs::Spec {
        fs::Spec::new(
            format!("{protocol}-{runtime}").parse().unwrap(),
            protocol,
            runtime,
            PathBuf::from("/tmp/x"),
        )
        .unwrap()
    }

    #[test]
    fn filesystem_offer_excludes_already_attached_pairs() {
        let recommended = vec![(fs::Protocol::Nfs, fs::Runtime::Host)];
        let offer = filesystem_offer(&recommended, &[]);
        assert_eq!(offer, recommended);

        let attached = [spec(fs::Protocol::Nfs, fs::Runtime::Host)];
        let offer = filesystem_offer(&recommended, &attached);
        assert!(offer.is_empty());
    }

    #[test]
    fn filesystem_offer_question_names_the_os_and_locations() {
        let offer = vec![(fs::Protocol::Nfs, fs::Runtime::Host)];
        let locations = vec!["/Users/raulk/omnifs".to_owned()];
        let question = filesystem_offer_question(&offer, &locations);
        assert_eq!(
            question,
            format!(
                "Attach the recommended filesystem for {} (nfs at /Users/raulk/omnifs)?",
                os_label()
            )
        );
    }

    // -- resolve_offer_decision: non-prompting branches ----------------------

    #[test]
    fn resolve_offer_decision_yes_always_accepts_without_prompting() {
        let output = Output::new(crate::ui::output::OutputMode::Human, false).with_yes(true);
        let prompt = PromptMode::for_test(true, true, false);
        assert!(resolve_offer_decision(&output, prompt, "Mount?").unwrap());
    }

    #[test]
    fn resolve_offer_decision_declines_without_prompting_when_no_input_or_non_interactive() {
        let output = Output::new(crate::ui::output::OutputMode::Human, false);
        for prompt in [
            PromptMode::for_test(true, false, true),
            PromptMode::for_test(false, false, false),
        ] {
            assert!(!resolve_offer_decision(&output, prompt, "Mount?").unwrap());
        }
    }

    // -- next block -----------------------------------------------------------

    #[test]
    fn next_block_lines_uses_a_real_unmounted_provider_when_one_exists() {
        let lines = next_block_lines(Some("github"));
        assert!(
            lines[0].starts_with("omnifs mount add github"),
            "{:?}",
            lines[0]
        );
        assert!(lines[0].contains("mount a service"));
        assert!(lines[1].starts_with("omnifs status"));
        assert!(lines[2].starts_with("omnifs fs ls"));
    }

    #[test]
    fn next_block_lines_falls_back_to_the_bare_command_without_an_example() {
        let lines = next_block_lines(None);
        assert!(
            lines[0].starts_with("omnifs mount add") && !lines[0].starts_with("omnifs mount add g"),
            "{:?}",
            lines[0]
        );
        assert!(lines[0].contains("mount a service"));
    }

    // -- closing sentence -------------------------------------------------------

    #[test]
    fn closing_sentence_browses_when_mounted_and_attached() {
        let mounted = vec!["dns".to_owned()];
        let location = PathBuf::from("/Users/raulk/omnifs");
        let sentence = closing_sentence(&mounted, Some(&location), None, Duration::from_secs(6));
        assert_eq!(
            sentence,
            "All set in 6s. Browse:  `ls /Users/raulk/omnifs/dns`"
        );
    }

    #[test]
    fn closing_sentence_points_at_mount_add_when_attached_without_new_mounts() {
        let location = PathBuf::from("/Users/raulk/omnifs");
        let sentence = closing_sentence(&[], Some(&location), None, Duration::from_secs(6));
        assert_eq!(
            sentence,
            "All set in 6s. Add a service:  `omnifs mount add`"
        );
    }

    #[test]
    fn closing_sentence_points_at_fs_attach_when_mounted_without_a_host_attach() {
        let mounted = vec!["dns".to_owned()];
        let id: fs::Id = "nfs-host".parse().unwrap();
        let sentence = closing_sentence(&mounted, None, Some(&id), Duration::from_secs(6));
        assert_eq!(
            sentence,
            "All set in 6s. Mount files:  `omnifs fs attach --name nfs-host`"
        );
    }

    #[test]
    fn closing_sentence_is_plain_when_nothing_happened() {
        assert_eq!(
            closing_sentence(&[], None, None, Duration::from_secs(6)),
            "All set in 6s."
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
