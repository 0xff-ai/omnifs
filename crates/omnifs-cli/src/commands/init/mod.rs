//! `omnifs init` — interactive setup for a new mount.
//!
//! Walks the user through naming a mount, discovers provider defaults from
//! the built-in catalog or provider wasm metadata, writes the resulting mount config to
//! the resolved omnifs config file, and runs the provider's default auth flow
//! when one is declared.

mod auth_import;
mod detect;
mod mount_file;
mod provider_selection;
mod spec_creation;
mod token_validation;

use crate::error::WithHint;
use anyhow::{Context, anyhow};
use clap::Args;
use omnifs_workspace::creds::{CredentialEntry, CredentialStore, FileStore};
use omnifs_workspace::mounts::{Name as MountName, Registry, UpgradePlan};
use omnifs_workspace::provider::{Catalog, ProviderManifest};
use secrecy::{ExposeSecret, SecretString};
use std::path::Path;
use time::OffsetDateTime;

use crate::auth::AuthSelection;
use crate::credential_target::CredentialTarget;
use crate::mount_config::MountConfig;
use crate::token_source::TokenSource;
use crate::workspace::Workspace;
pub(crate) use auth_import::AuthImportDecision;
use mount_file::MountFile;
use omnifs_workspace::layout::WorkspaceLayout;
use provider_selection::ProviderSelection;
use spec_creation::MountSpecCreator;
use token_validation::StaticTokenValidator;

#[derive(Args, Debug, Clone)]
#[allow(clippy::struct_excessive_bools)] // mirrors CLI flags 1:1
pub struct InitArgs {
    /// Provider to use (positional; picker if omitted).
    pub provider: Option<String>,
    /// Mount name override. Auto-generated from the provider if absent.
    #[arg(long = "as")]
    pub as_name: Option<String>,
    /// Skip prompts. Static-token providers also require --token or --token-env.
    #[arg(long)]
    pub no_input: bool,
    /// Accept the auto-suggested mount name on collision (never overwrite).
    #[arg(long)]
    pub yes: bool,
    /// Print the OAuth URL instead of opening a browser.
    #[arg(long)]
    pub no_browser: bool,
    /// Read the static token from this source. Use `-` for stdin.
    #[arg(long, conflicts_with = "token_env")]
    pub token: Option<String>,
    /// Read the static token from this environment variable.
    #[arg(long, value_name = "ENV_VAR", conflicts_with = "token")]
    pub token_env: Option<String>,
    /// OAuth scope to request. Repeat for multiple scopes.
    #[arg(long = "scope")]
    pub scopes: Vec<String>,
}

impl InitArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        let workspace = Workspace::resolve()?;
        self.run_in_workspace(&workspace).await
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) async fn run_in_workspace(self, workspace: &Workspace) -> anyhow::Result<()> {
        let paths = workspace.layout();
        crate::provider_bundle::ensure_providers_installed(&paths.providers_dir)?;
        let interactive = !self.no_input;
        let catalog = workspace.catalog();
        let mounts = workspace.mounts()?;
        let installed = crate::catalog::installed_providers(catalog)?;
        if installed.is_empty() {
            anyhow::bail!("no built-in or disk providers are available");
        }

        let provider_selection = ProviderSelection::new(&mounts, &installed);
        let (provider_name, mount_name) = provider_selection.resolve(
            self.provider.as_deref(),
            self.as_name.as_deref(),
            interactive,
            self.yes,
        )?;

        let (provider, manifest) = crate::catalog::find_installed(&installed, &provider_name)
            .ok_or_else(|| {
                anyhow!(
                    "provider `{provider_name}` not found; available: {}",
                    provider_selection.provider_names().join(", ")
                )
            })
            .with_hint("Run `omnifs init` (no args) to see the picker of available providers")
            .with_hint(format!(
                "Or run `omnifs providers add <wasm-or-dir>` to install provider artifacts into {}",
                paths.providers_dir.display()
            ))?;
        let reference = provider.reference();
        let existing_mount = mounts.iter().find(|mount| mount.name == mount_name);
        let upgrade_approval = match existing_mount {
            Some(existing) => approved_upgrade_for_existing_mount(
                catalog,
                existing,
                manifest,
                &provider_name,
                &mount_name,
                interactive,
            )?,
            None => None,
        };
        let auth_manifest = manifest.wasm_auth_manifest();
        let default_auth = AuthSelection::from_provider_default(&reference, &mount_name, manifest);
        if interactive {
            print_capability_justifications(manifest);
        }
        if self.no_input && default_auth.as_ref().is_some_and(AuthSelection::is_oauth) {
            anyhow::bail!(
                "`omnifs init --no-input` cannot complete OAuth. Run `omnifs init {provider_name}` interactively."
            );
        }
        let creator = MountSpecCreator::new(&reference, &mount_name, manifest);
        if self.no_input && creator.requires_prompt() {
            anyhow::bail!(
                "`omnifs init --no-input` cannot complete provider config prompts for `{provider_name}`. Run `omnifs init {provider_name}` interactively."
            );
        }
        let created = creator.create(interactive)?;

        // Resolve the effective auth before writing the mount config. If the
        // mount's default auth is OAuth and the user accepts an ambient
        // credential (gh CLI token, GITHUB_TOKEN env), we promote the mount
        // to a static-token mount using a static scheme from the manifest.
        // Storing under the OAuth scheme would make `omnifs up` later apply
        // OAuth header semantics (Bearer prefix, scope handling) to a plain
        // PAT, which breaks providers like Linear whose static scheme uses
        // no header prefix.
        let import_outcome = AuthImportDecision::new(
            default_auth,
            auth_manifest.as_ref(),
            &provider_name,
            interactive,
            self.yes,
        )
        .resolve()?;
        let effective_auth = import_outcome.auth.clone();

        let mount_file = MountFile::new(
            &mount_name,
            &reference,
            effective_auth.as_ref(),
            &self.scopes,
            created,
        );
        let spec = mount_file.into_spec();
        let mount_path = paths.mounts_dir.join(format!("{mount_name}.json"));
        let daemon_report = match if existing_mount.is_some() {
            workspace
                .daemon()
                .update_mount_if_ready(&spec, upgrade_approval.as_ref())
                .await
        } else {
            workspace.daemon().create_mount_if_ready(&spec).await
        } {
            Ok(Some(report)) => {
                anstream::eprintln!("✓ Wrote {}", WorkspaceLayout::display(&mount_path));
                Some(report)
            },
            Ok(None) => {
                Registry::load(&paths.mounts_dir)?.put(&spec)?;
                anstream::eprintln!("✓ Wrote {}", WorkspaceLayout::display(&mount_path));
                None
            },
            Err(error) => {
                anstream::eprintln!(
                    "Running daemon could not save mount `{mount_name}`: {error:#}"
                );
                anstream::eprintln!("Falling back to a local mount config write.");
                Registry::load(&paths.mounts_dir)?.put(&spec)?;
                anstream::eprintln!("✓ Wrote {}", WorkspaceLayout::display(&mount_path));
                None
            },
        };

        if let Some(auth) = effective_auth.as_ref() {
            if let Some(token) = import_outcome.token {
                run_static_token_init(manifest, auth, token, &paths.credentials_file).await?;
            } else if auth.is_oauth() {
                anstream::eprintln!("Starting OAuth login for `{mount_name}` ...");
                crate::auth::login_with_workspace(
                    workspace,
                    mount_name.as_str(),
                    auth.account.as_deref(),
                    self.no_browser,
                    &self.scopes,
                )
                .await
                .inspect_err(|_| {
                    anstream::eprintln!(
                        "Mount `{mount_name}` was created, but login did not complete. Run `omnifs mounts reauth {mount_name}` to finish."
                    );
                })?;
            } else {
                if interactive && let Ok(scheme) = auth.static_token_scheme(manifest) {
                    let guidance = manifest
                        .auth
                        .as_ref()
                        .map(|auth| auth.guidance_for(&scheme.key))
                        .unwrap_or_default();
                    anstream::eprintln!();
                    anstream::eprintln!("Authenticating `{mount_name}` with a static token:");
                    crate::auth::explain::render_static_token_intro(
                        scheme.creation_url.as_deref(),
                        &guidance,
                    );
                }
                let source = TokenSource::resolve(
                    self.token.as_deref(),
                    self.token_env.as_deref(),
                    interactive,
                )?;
                let token = source.read()?;
                run_static_token_init(manifest, auth, token, &paths.credentials_file).await?;
            }
        }

        anstream::eprintln!();
        anstream::eprintln!("Mount `{mount_name}` is ready.");

        match daemon_report {
            Some(report) if report.failure.is_none() => {
                anstream::eprintln!("✓ Applied to the running daemon");
            },
            Some(report) => {
                let reason = report
                    .failure
                    .as_ref()
                    .map_or("unknown error", |failure| failure.reason.as_str());
                anstream::eprintln!(
                    "Mount config saved, but loading it into the running daemon failed: {reason}"
                );
                anstream::eprintln!("Run `omnifs up` to restart with the new mount.");
            },
            None => anstream::eprintln!("Run `omnifs up` to start it."),
        }
        crate::telemetry::maybe_print_health_nudge(workspace).await;
        Ok(())
    }
}

fn approved_upgrade_for_existing_mount(
    catalog: &Catalog,
    existing: &MountConfig,
    candidate_manifest: &ProviderManifest,
    provider_name: &str,
    mount_name: &MountName,
    interactive: bool,
) -> anyhow::Result<Option<UpgradePlan>> {
    let existing_provider = existing.config.provider_name();
    if existing_provider.as_str() != provider_name {
        anyhow::bail!(
            "mount `{mount_name}` already exists for provider `{existing_provider}`; remove it first or choose a different name"
        );
    }

    let Some(pinned) = catalog
        .get(&existing.config.provider.id)
        .with_context(|| format!("load pinned provider for mount `{mount_name}`"))?
    else {
        anyhow::bail!(
            "mount `{mount_name}` pinned provider artifact {id} is missing; cannot compute an upgrade approval",
            id = existing.config.provider.id,
        );
    };
    let pinned_manifest = pinned
        .manifest()
        .with_context(|| format!("read pinned provider manifest for mount `{mount_name}`"))?;
    let plan = UpgradePlan::diff(&pinned_manifest, candidate_manifest);
    if !plan.requires_approval() {
        return Ok(None);
    }
    if !interactive {
        anyhow::bail!(
            "`omnifs init --no-input` cannot approve provider upgrade changes for existing mount `{mount_name}`"
        );
    }

    anstream::println!();
    anstream::println!(
        "Mount `{mount_name}` already exists. `{provider_name}` changed its provider surface:"
    );
    for change in crate::upgrade::describe_upgrade_changes(&plan) {
        anstream::println!("  - {change}");
    }
    let approved = inquire::Confirm::new("Approve this provider upgrade?")
        .with_default(false)
        .prompt()
        .map_err(|error| anyhow!("confirm prompt: {error}"))?;
    if !approved {
        anyhow::bail!("aborted");
    }
    Ok(Some(plan))
}

pub(crate) fn print_capability_justifications(manifest: &ProviderManifest) {
    let limits = crate::capability::limit_lines(&manifest.limits);
    if manifest.capabilities.is_empty() && limits.is_empty() {
        return;
    }

    if !manifest.capabilities.is_empty() {
        anstream::println!();
        anstream::println!("{}", crate::style::bold("Provider capabilities"));
        anstream::println!("{} requires:", manifest.display_name);
        for entry in &manifest.capabilities {
            anstream::println!(
                "  • {}: {}",
                crate::capability::capability_label(entry),
                crate::capability::capability_value(entry)
            );
            anstream::println!("    {}", crate::style::dim(entry.why()));
        }
    }

    if !limits.is_empty() {
        anstream::println!();
        anstream::println!("{}", crate::style::bold("Provider limits"));
        anstream::println!("{} requests:", manifest.display_name);
        for line in limits {
            anstream::println!("  • {}: {}", line.label, line.value);
            anstream::println!("    {}", crate::style::dim(line.why));
        }
    }
}

pub(crate) async fn run_static_token_init(
    manifest: &ProviderManifest,
    auth: &AuthSelection,
    token: SecretString,
    credentials_file: &Path,
) -> anyhow::Result<CredentialTarget> {
    let static_token_scheme = auth.static_token_scheme(manifest)?;

    let header_name = static_token_scheme
        .header_name
        .as_deref()
        .unwrap_or("Authorization");
    let header_prefix = static_token_scheme.value_prefix.as_str();

    let validation = match static_token_scheme.validation.as_ref() {
        Some(v) => Some(
            StaticTokenValidator::new(v, header_name, header_prefix)
                .validate(token.expose_secret())
                .await?,
        ),
        None => None,
    };
    if let Some(outcome) = &validation {
        if let Some(identity) = &outcome.identity {
            anstream::eprintln!("✓ Authenticated as {identity}");
        } else {
            anstream::eprintln!("✓ Token accepted");
        }
        if let Some(workspace) = &outcome.workspace {
            anstream::eprintln!("✓ Workspace: {workspace}");
        }
    }

    let store = FileStore::new(credentials_file);
    anstream::eprintln!("Storing credential in {} ...", store.backend_label());
    let now = OffsetDateTime::now_utc();
    let mut entry = CredentialEntry::static_token(token, now);
    entry.set_last_validated(validation.as_ref().map(|_| now));
    entry.set_upstream_identity(validation.as_ref().and_then(|o| o.identity.clone()));
    entry.set_extras(
        validation
            .as_ref()
            .map(|o| o.extras.clone())
            .unwrap_or_default(),
    );
    let auth_manifest = manifest
        .auth
        .as_ref()
        .map(omnifs_workspace::provider::ProviderAuthManifest::wasm_auth_manifest);
    let scheme_key = crate::auth::AuthManifestView::new(auth_manifest.as_ref())
        .static_token_scheme_key(auth.scheme.as_deref(), None)?;
    let target =
        CredentialTarget::for_static_import(&manifest.id, &scheme_key, auth.account.as_deref())?;
    for key in target.keys() {
        store
            .put(key, &entry)
            .with_context(|| "failed to store credential")?;
    }
    anstream::eprintln!("✓ Stored");
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::spec_creation::{CreatedMountSpec, MountSpecCreator};
    use super::{AuthImportDecision, InitArgs, MountFile};
    use crate::auth::AuthSelection;
    use crate::workspace::Workspace;
    use omnifs_caps::{
        Grant, Grants as ProviderCapabilities, LimitDeclarations, Limits as ProviderLimits,
        PreopenMode, PreopenedPath, ResourceLimit,
    };
    use omnifs_workspace::authn::{AuthManifest, AuthScheme};
    use omnifs_workspace::ids::{ProviderId, ProviderMeta, ProviderName, ProviderRef};
    use omnifs_workspace::mounts::Name as MountName;
    use omnifs_workspace::mounts::Registry;
    use omnifs_workspace::provider::{
        Catalog, ConfigField, ConfigMetadata, ConfigType, HostResourceBinding, ProviderManifest,
        ProviderStore,
    };
    use serde_json::Value;

    #[test]
    fn generate_mount_config_materializes_config_defaults() {
        let mut manifest = provider_manifest();
        manifest.config = Some(ConfigMetadata {
            fields: vec![ConfigField {
                name: "endpoint".to_string(),
                value_type: ConfigType::String,
                required: true,
                default: Some(serde_json::json!("unix:///var/run/docker.sock")),
                description: None,
                binding: None,
            }],
        });

        let reference = provider_ref("linear");
        let mount_name = MountName::try_from("linear").unwrap();
        let created = MountSpecCreator::new(&reference, &mount_name, &manifest)
            .create(false)
            .unwrap();

        assert_eq!(
            created.config,
            Some(serde_json::json!({"endpoint": "unix:///var/run/docker.sock"})),
        );
        // `init` seeds the spec's grants from the manifest's needs, so a mount
        // carries explicit grants the materialize-time check can satisfy.
        let capabilities = created.capabilities.expect("grants seeded from needs");
        assert_eq!(
            capabilities.domains,
            Some(Grant::Literal(vec!["api.linear.app".to_string()])),
        );
        assert_eq!(
            created.limits.expect("limits seeded from manifest"),
            ProviderLimits {
                max_memory_mb: Some(128),
                ..ProviderLimits::default()
            }
        );
    }

    #[test]
    fn config_metadata_reports_interactive_prompt_requirement() {
        let mut manifest = provider_manifest();
        manifest.config = Some(ConfigMetadata {
            fields: vec![ConfigField {
                name: "path".to_string(),
                value_type: ConfigType::String,
                required: false,
                default: Some(serde_json::json!("/data/test.db")),
                description: None,
                binding: Some(HostResourceBinding::File {
                    mode: PreopenMode::Ro,
                }),
            }],
        });

        let reference = provider_ref("linear");
        let mount_name = MountName::try_from("linear").unwrap();
        assert!(MountSpecCreator::new(&reference, &mount_name, &manifest).requires_prompt());
    }

    #[test]
    fn mount_file_includes_generated_config_and_capabilities() {
        let dir = tempfile::tempdir().unwrap();
        let mounts = dir.path();

        let spec = MountFile::new(
            &MountName::try_from("db").unwrap(),
            &provider_ref("db"),
            None,
            &[],
            CreatedMountSpec {
                config: Some(serde_json::json!({"path": "/data/chinook.db"})),
                capabilities: Some(ProviderCapabilities {
                    preopened_paths: Some(Grant::Literal(vec![PreopenedPath {
                        host: "/host/db".to_string(),
                        guest: "/data".to_string(),
                        mode: PreopenMode::Ro,
                    }])),
                    ..ProviderCapabilities::default()
                }),
                limits: Some(ProviderLimits {
                    max_memory_mb: Some(128),
                    ..ProviderLimits::default()
                }),
            },
        )
        .into_spec();

        Registry::load(mounts).unwrap().put(&spec).unwrap();

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(mounts.join("db.json")).unwrap())
                .unwrap();
        assert_eq!(written["config"]["path"], "/data/chinook.db");
        assert_eq!(
            written["capabilities"]["preopened_paths"][0]["host"],
            "/host/db"
        );
        assert_eq!(written["limits"]["max_memory_mb"], 128);
    }

    #[test]
    fn init_dns_writes_snapshot_spec() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::from_layout(
            omnifs_workspace::layout::WorkspaceLayout::under_root(dir.path()),
        );
        let args = InitArgs {
            provider: Some("dns".to_string()),
            as_name: None,
            no_input: true,
            yes: true,
            no_browser: true,
            token: None,
            token_env: None,
            scopes: Vec::new(),
        };

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(args.run_in_workspace(&workspace))
            .unwrap();

        let spec = std::fs::read_to_string(workspace.layout().mounts_dir.join("dns.json")).unwrap();
        // The provider content id hashes the built wasm, which differs across
        // build environments; normalize it before the byte comparison.
        let parsed: serde_json::Value = serde_json::from_str(&spec).unwrap();
        let id = parsed["provider"]["id"].as_str().unwrap();
        assert_eq!(id.len(), 64, "content id must be 64 hex chars: {id}");
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
        let normalized = spec.replace(id, "<PROVIDER_ID>");
        assert_eq!(normalized, include_str!("snapshots/init_dns_spec.json"));
    }

    #[test]
    #[allow(unsafe_code)] // env::set_var/remove_var require unsafe; guarded by lock_env().
    fn import_outcome_promotes_oauth_default_to_static_when_token_imported() {
        // Simulate an OAuth-default mount (linear) where the user has
        // LINEAR_API_KEY in env. `--yes` accepts the ambient credential.
        // Saved on a per-test guard so concurrent tests don't see leaks.
        let _guard = lock_env();
        // SAFETY: env mutation is isolated by the lock_env() guard above,
        // which serializes with any other test touching this env var.
        unsafe {
            std::env::set_var("LINEAR_API_KEY", "lin_api_xxx");
        }
        let auth_manifest = AuthManifest {
            schemes: vec![
                AuthScheme::StaticToken(omnifs_workspace::authn::StaticTokenScheme {
                    key: "pat".to_string(),
                    header_name: Some("Authorization".to_string()),
                    value_prefix: String::new(),
                    description: "Linear API key".to_string(),
                    inject_domains: vec![],
                    creation_url: None,
                    validation: None,
                    ambient_sources: vec![omnifs_workspace::authn::AmbientSource::env_var(
                        "LINEAR_API_KEY",
                    )],
                }),
                AuthScheme::Oauth(omnifs_workspace::authn::OauthScheme {
                    key: "oauth".to_string(),
                    display_name: "Linear OAuth".to_string(),
                    authorization_endpoint: "https://example.com/authorize".to_string(),
                    token_endpoint: "https://example.com/token".to_string(),
                    revocation_endpoint: None,
                    default_client_id: None,
                    default_scopes: vec![],
                    flow: omnifs_workspace::authn::OAuthFlow::PkceLoopback(
                        omnifs_workspace::authn::PkceLoopbackConfig {
                            redirect_uri_template: "http://127.0.0.1:{port}/cb".to_string(),
                        },
                    ),
                    token_endpoint_auth: omnifs_workspace::authn::TokenEndpointAuthMethod::None,
                    refresh_token_rotates: false,
                    extra_authorize_params: vec![],
                    extra_token_params: vec![],
                    inject_domains: vec![],
                    inject_header_name: None,
                    inject_value_prefix: String::new(),
                }),
            ],
        };
        let oauth_default = AuthSelection {
            auth_type: omnifs_workspace::authn::AuthKind::OAuth,
            scheme: Some("oauth".to_string()),
            account: None,
        };

        let outcome = AuthImportDecision::new(
            Some(oauth_default),
            Some(&auth_manifest),
            "linear",
            true,
            true,
        )
        .resolve()
        .unwrap();

        let promoted = outcome.auth.expect("auth");
        assert_eq!(
            promoted.auth_type,
            omnifs_workspace::authn::AuthKind::StaticToken
        );
        assert_eq!(promoted.scheme.as_deref(), Some("pat"));
        assert!(outcome.token.is_some(), "imported token should be set");

        // SAFETY: env mutation is isolated by the lock_env() guard above.
        unsafe {
            std::env::remove_var("LINEAR_API_KEY");
        }
    }

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn provider_ref(name: &str) -> ProviderRef {
        ProviderRef {
            id: ProviderId::from_wasm_bytes(name.as_bytes()),
            meta: ProviderMeta {
                name: ProviderName::new(name).unwrap(),
                version: None,
            },
        }
    }

    /// Templates are drawn from the latest installed artifact in the
    /// content-addressed store: install one and assert it surfaces with its
    /// embedded manifest and a pinnable reference.
    #[test]
    fn installed_providers_reads_latest_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let paths = omnifs_workspace::layout::WorkspaceLayout::under_root(dir.path());

        let mut manifest = provider_manifest();
        manifest.default_mount = "linear-dev".to_owned();
        let wasm = wasm_with_custom_section(
            omnifs_workspace::provider::PROVIDER_METADATA_SECTION_NAME,
            &serde_json::to_vec(&manifest).unwrap(),
        );
        let id = ProviderId::from_wasm_bytes(&wasm);
        let store = ProviderStore::new(&paths.providers_dir);
        store.put_if_absent(&id, &wasm).unwrap();
        store
            .install(
                id,
                ProviderMeta {
                    name: ProviderName::new("linear").unwrap(),
                    version: None,
                },
                "omnifs_provider_linear.wasm".into(),
            )
            .unwrap();

        let installed =
            crate::catalog::installed_providers(&Catalog::open(&paths.providers_dir)).unwrap();

        let (provider, manifest) = installed
            .iter()
            .find(|(provider, _)| provider.meta.name.as_str() == "linear")
            .expect("linear provider");
        assert_eq!(manifest.default_mount, "linear-dev");
        assert_eq!(provider.reference().id, id);
        assert_eq!(provider.reference().meta.name.as_str(), "linear");
    }

    fn provider_manifest() -> ProviderManifest {
        use omnifs_workspace::authn::{
            AuthScheme, OAuthFlow, OauthScheme, PkceLoopbackConfig, StaticTokenScheme,
            TokenEndpointAuthMethod,
        };
        use omnifs_workspace::provider::ProviderAuthManifest;
        use std::collections::BTreeMap;

        let domains = vec!["api.linear.app".to_string()];
        ProviderManifest {
            id: "linear".to_string(),
            display_name: "Linear".to_string(),
            provider: "omnifs_provider_linear.wasm".to_string(),
            default_mount: "linear".to_string(),
            version: None,
            wit_package: None,
            sdk_version: None,
            capabilities: vec![omnifs_caps::AccessNeed::Domain {
                value: "api.linear.app".to_string(),
                why: "api calls".to_string(),
                dynamic: false,
            }],
            limits: LimitDeclarations {
                max_memory_mb: Some(ResourceLimit {
                    value: 128,
                    why: "in-memory caching".to_string(),
                }),
                ..LimitDeclarations::default()
            },
            auth: Some(ProviderAuthManifest {
                default: "oauth".to_string(),
                guidance: BTreeMap::new(),
                schemes: vec![
                    AuthScheme::Oauth(OauthScheme {
                        key: "oauth".to_string(),
                        display_name: "Linear OAuth".to_string(),
                        authorization_endpoint: "https://linear.app/oauth/authorize".to_string(),
                        token_endpoint: "https://api.linear.app/oauth/token".to_string(),
                        revocation_endpoint: None,
                        default_client_id: Some("test-client-id".to_string()),
                        default_scopes: vec!["read".to_string()],
                        flow: OAuthFlow::PkceLoopback(PkceLoopbackConfig {
                            redirect_uri_template: "http://127.0.0.1:{port}/callback".to_string(),
                        }),
                        token_endpoint_auth: TokenEndpointAuthMethod::None,
                        refresh_token_rotates: true,
                        extra_authorize_params: vec![],
                        extra_token_params: vec![],
                        inject_domains: domains.clone(),
                        inject_header_name: Some("Authorization".to_string()),
                        inject_value_prefix: String::new(),
                    }),
                    AuthScheme::StaticToken(StaticTokenScheme {
                        key: "pat".to_string(),
                        header_name: Some("Authorization".to_string()),
                        value_prefix: String::new(),
                        description: "Linear API key".to_string(),
                        inject_domains: domains.clone(),
                        creation_url: None,
                        validation: None,
                        ambient_sources: Vec::new(),
                    }),
                ],
            }),
            config: None,
        }
    }

    fn wasm_with_custom_section(name: &str, data: &[u8]) -> Vec<u8> {
        let mut wasm = b"\0asm\x01\0\0\0".to_vec();
        wasm.push(0);
        let mut payload = Vec::new();
        encode_var_u32(name.len(), &mut payload);
        payload.extend_from_slice(name.as_bytes());
        payload.extend_from_slice(data);
        encode_var_u32(payload.len(), &mut wasm);
        wasm.extend_from_slice(&payload);
        wasm
    }

    fn encode_var_u32(mut value: usize, out: &mut Vec<u8>) {
        loop {
            let mut byte = u8::try_from(value & 0x7f).unwrap();
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if value == 0 {
                break;
            }
        }
    }
}
