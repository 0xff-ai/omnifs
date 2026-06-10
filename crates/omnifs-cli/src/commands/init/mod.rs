//! `omnifs init` — interactive setup for a new mount.
//!
//! Walks the user through naming a mount, discovers provider defaults from
//! the built-in catalog or provider wasm metadata, writes the resulting mount config to
//! `~/.omnifs/config.toml`, and runs the provider's default auth flow
//! when one is declared.

mod auth_import;
mod config_generation;
pub mod detect;
mod mount_file;
mod provider_selection;
mod token_validation;

use crate::error::WithHint;
use crate::session::CredsBackend;
use anyhow::{Context, anyhow};
use clap::Args;
use omnifs_creds::CredentialEntry;
use omnifs_provider::ProviderManifest;
use secrecy::{ExposeSecret, SecretString};
use std::path::{Path, PathBuf};
use time::OffsetDateTime;

use crate::app_context::AppContext;
use crate::auth::AuthSelection;
use crate::commands::auth;
use crate::credential_target::CredentialTarget;
use crate::paths::{PathOverrides, Paths};
use crate::token_source::TokenSource;
use auth_import::AuthImportDecision;
use config_generation::MountConfigGenerator;
use mount_file::MountFile;
use provider_selection::ProviderSelection;
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
    /// Override the directory holding provider WASM components.
    ///
    /// Defaults to `OMNIFS_PROVIDERS_DIR`, then the default config providers dir.
    #[arg(long)]
    pub providers_dir: Option<PathBuf>,
    /// Print the provider capability table at the start of the flow.
    /// Setup-driven runs suppress this because the picker already showed it.
    #[arg(skip = true)]
    pub show_capabilities: bool,
}

impl InitArgs {
    #[allow(clippy::too_many_lines)]
    pub async fn run(self) -> anyhow::Result<()> {
        let ctx = AppContext::resolve(
            PathOverrides {
                providers_dir: self.providers_dir.clone(),
                ..Default::default()
            },
            None,
            None,
        )?;
        let paths = ctx.paths();
        let interactive = !self.no_input;
        let providers_dir = paths.providers_dir.clone();
        let catalog = ctx.catalog();
        let workspace = ctx.workspace();
        let mounts = workspace.mounts()?;
        let templates = catalog.provider_templates()?;
        if templates.is_empty() {
            anyhow::bail!("no built-in or disk providers are available");
        }

        let provider_selection = ProviderSelection::new(&mounts, &templates);
        let (provider_name, mount_name) = provider_selection.resolve(
            self.provider.as_deref(),
            self.as_name.as_deref(),
            interactive,
            self.yes,
        )?;

        let template = templates
            .get(&provider_name)
            .ok_or_else(|| {
                anyhow!(
                    "provider `{provider_name}` not found; available: {}",
                    provider_selection.provider_names().join(", ")
                )
            })
            .with_hint("Run `omnifs init` (no args) to see the picker of available providers")
            .with_hint(format!(
                "Or place a provider wasm in {}",
                providers_dir.display()
            ))?;
        let default_auth = AuthSelection::from_provider_default(&template.manifest);
        if interactive && self.show_capabilities {
            print_capability_justifications(&template.manifest);
        }
        if self.no_input && default_auth.as_ref().is_some_and(AuthSelection::is_oauth) {
            anyhow::bail!(
                "`omnifs init --no-input` cannot complete OAuth. Run `omnifs init {provider_name}` interactively, or create the mount and run `omnifs auth login {mount_name}`."
            );
        }
        let config_generator = MountConfigGenerator::new(&template.manifest);
        if self.no_input && config_generator.requires_prompt() {
            anyhow::bail!(
                "`omnifs init --no-input` cannot complete provider config prompts for `{provider_name}`. Run `omnifs init {provider_name}` interactively."
            );
        }
        let generated = config_generator.generate(interactive)?;

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
            template.auth_manifest.as_ref(),
            &provider_name,
            interactive,
            self.yes,
        )
        .resolve()?;
        let effective_auth = import_outcome.auth.clone();

        let spec = MountFile::new(
            &mount_name,
            &template.manifest,
            effective_auth.as_ref(),
            &self.scopes,
            generated,
        )
        .into_spec()?;
        workspace.upsert_mount(&spec)?;
        anstream::println!("✓ Updated {}", Paths::display(&paths.config_file));

        if let Some(auth) = effective_auth.as_ref() {
            if let Some(token) = import_outcome.token {
                run_static_token_init(&template.manifest, auth, token, &paths.credentials_file)
                    .await?;
            } else if auth.is_oauth() {
                anstream::println!("Starting OAuth login for `{mount_name}` ...");
                auth::login_with_paths(
                    paths.config_dir.clone(),
                    providers_dir.clone(),
                    paths.credentials_file.clone(),
                    mount_name.as_str(),
                    auth.account.as_deref(),
                    self.no_browser,
                    &self.scopes,
                )
                .await
                .inspect_err(|_| {
                    anstream::eprintln!(
                        "Mount `{mount_name}` was created, but login did not complete. Run `omnifs auth login {mount_name}` to finish."
                    );
                })?;
            } else {
                let source = TokenSource::resolve(
                    self.token.as_deref(),
                    self.token_env.as_deref(),
                    interactive,
                )?;
                let token = source.read()?;
                run_static_token_init(&template.manifest, auth, token, &paths.credentials_file)
                    .await?;
            }
        }

        anstream::println!();
        anstream::println!("Mount `{mount_name}` is ready.");

        let config = crate::session::MountConfig::from_parsed(spec, paths.config_file.clone())?;
        let store = CredsBackend::auto(&paths.credentials_file, false);
        match crate::live::add_mount(
            ctx.runtime().container_name(),
            catalog,
            store.as_ref(),
            config,
        )
        .await
        {
            Ok(crate::live::LiveApply::Applied) => {
                anstream::println!("✓ Loaded into the running daemon");
            },
            Ok(crate::live::LiveApply::NotRunning) => {
                anstream::println!("Run `omnifs up` to start it.");
            },
            Ok(crate::live::LiveApply::RestartRequired(reason)) => {
                anstream::println!(
                    "A daemon is running, but this mount can't load live ({reason}). Run `omnifs up` to restart with it."
                );
            },
            Err(error) => {
                anstream::eprintln!(
                    "Mount config saved, but loading it into the running daemon failed: {error:#}"
                );
                anstream::eprintln!("Run `omnifs up` to restart with the new mount.");
            },
        }
        Ok(())
    }
}

pub(crate) fn print_capability_justifications(manifest: &ProviderManifest) {
    if manifest.capabilities.is_empty() {
        return;
    }

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

async fn run_static_token_init(
    manifest: &ProviderManifest,
    auth: &AuthSelection,
    token: SecretString,
    credentials_file: &Path,
) -> anyhow::Result<()> {
    let (static_token_scheme, inject) = auth.static_token_scheme(manifest)?;

    let header_name = &inject.header;
    let header_prefix = &inject.prefix;

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
            anstream::println!("✓ Authenticated as {identity}");
        } else {
            anstream::println!("✓ Token accepted");
        }
        if let Some(workspace) = &outcome.workspace {
            anstream::println!("✓ Workspace: {workspace}");
        }
    }

    let store = CredsBackend::auto(credentials_file, true);
    anstream::println!("Storing credential in {} ...", store.backend_label());
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
        .map(omnifs_provider::ProviderAuthManifest::wasm_auth_manifest);
    let scheme_key = crate::auth::AuthManifestView::new(auth_manifest.as_ref())
        .static_token_scheme_key(auth.scheme.as_deref(), None)?;
    let target =
        CredentialTarget::for_static_import(&manifest.id, &scheme_key, auth.account.as_deref())?;
    for key in target.keys() {
        store
            .put(key, &entry)
            .with_context(|| "failed to store credential")?;
    }
    anstream::println!("✓ Stored");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::config_generation::{GeneratedMountConfig, MountConfigGenerator};
    use super::{AuthImportDecision, MountFile};
    use crate::auth::AuthSelection;
    use crate::catalog::{ProviderCatalog, ProviderSource};
    use omnifs_core::MountName;
    use omnifs_provider::{
        AuthManifest, AuthScheme, InitHint, InitInput, PreopenMode, PreopenStrategy, PreopenedPath,
        ProviderCapabilities, ProviderManifest,
    };
    use serde_json::Value;
    use tempfile::TempDir;

    #[test]
    fn generate_mount_config_materializes_schema_defaults() {
        let mut manifest = provider_manifest();
        manifest.config_schema = Some(
            serde_json::from_value(serde_json::json!({
                "type": "object",
                "required": ["endpoint"],
                "properties": {
                    "endpoint": {
                        "type": "string",
                        "default": "unix:///var/run/docker.sock"
                    }
                }
            }))
            .unwrap(),
        );

        let generated = MountConfigGenerator::new(&manifest)
            .generate(false)
            .unwrap();

        assert_eq!(
            generated.config,
            Some(serde_json::json!({"endpoint": "unix:///var/run/docker.sock"})),
        );
        assert!(generated.capabilities.is_none());
    }

    #[test]
    fn config_schema_reports_interactive_prompt_requirement() {
        let mut manifest = provider_manifest();
        manifest.config_schema = Some(
            serde_json::from_value(serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "default": "/data/test.db",
                        "x-omnifs-init": {
                            "input": "host-file",
                            "guestDir": "/data"
                        }
                    }
                }
            }))
            .unwrap(),
        );

        assert!(MountConfigGenerator::new(&manifest).requires_prompt());
    }

    #[test]
    fn host_file_hint_derives_guest_config_and_preopen_capability() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("chinook.db");
        std::fs::write(&db, "").unwrap();
        let manifest = provider_manifest();
        let mut config = serde_json::json!({
            "database_type": "sqlite",
            "path": "/data/test.db",
            "read_only": true
        });
        let mut capabilities = None;

        MountConfigGenerator::new(&manifest)
            .apply_host_file_hint(
                "path",
                &InitHint {
                    input: Some(InitInput::HostFile),
                    guest_dir: Some("/data".to_string()),
                    preopen_mode: PreopenMode::Ro,
                    preopen_strategy: PreopenStrategy::Append,
                },
                &db,
                &mut config,
                &mut capabilities,
            )
            .unwrap();

        assert_eq!(config["path"], "/data/chinook.db");
        let capabilities = capabilities.unwrap();
        let expected_host = tmp.path().canonicalize().unwrap().display().to_string();
        assert_eq!(
            capabilities.preopened_paths,
            Some(vec![PreopenedPath {
                host: expected_host,
                guest: "/data".to_string(),
                mode: PreopenMode::Ro,
            }]),
        );
        assert_eq!(capabilities.max_memory_mb, Some(128));
    }

    #[test]
    fn host_file_hint_canonicalizes_and_replaces_preopens() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("db");
        std::fs::create_dir(&dir).unwrap();
        let db = dir.join("chinook.db");
        std::fs::write(&db, "").unwrap();
        let manifest = provider_manifest();
        let mut config = serde_json::json!({"path": "/data/test.db"});
        let mut capabilities = None;
        let noncanonical = dir.join("..").join("db").join("chinook.db");

        MountConfigGenerator::new(&manifest)
            .apply_host_file_hint(
                "path",
                &InitHint {
                    input: Some(InitInput::HostFile),
                    guest_dir: Some("/data".to_string()),
                    preopen_mode: PreopenMode::Ro,
                    preopen_strategy: PreopenStrategy::Replace,
                },
                &noncanonical,
                &mut config,
                &mut capabilities,
            )
            .unwrap();

        let capabilities = capabilities.unwrap();
        let expected_host = dir.canonicalize().unwrap().display().to_string();
        assert_eq!(config["path"], "/data/chinook.db");
        assert_eq!(
            capabilities.preopened_paths,
            Some(vec![PreopenedPath {
                host: expected_host,
                guest: "/data".to_string(),
                mode: PreopenMode::Ro,
            }]),
        );
        assert_eq!(capabilities.max_memory_mb, Some(128));
    }

    #[test]
    fn host_file_hint_dedupes_append_and_rejects_preopen_conflicts() {
        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        let first_db = first.path().join("first.db");
        let second_db = second.path().join("second.db");
        std::fs::write(&first_db, "").unwrap();
        std::fs::write(&second_db, "").unwrap();
        let manifest = provider_manifest();
        let hint = InitHint {
            input: Some(InitInput::HostFile),
            guest_dir: Some("/data".to_string()),
            preopen_mode: PreopenMode::Ro,
            preopen_strategy: PreopenStrategy::Append,
        };
        let mut config = serde_json::json!({"path": "/data/test.db"});
        let mut capabilities = None;

        let generator = MountConfigGenerator::new(&manifest);

        generator
            .apply_host_file_hint("path", &hint, &first_db, &mut config, &mut capabilities)
            .unwrap();

        let preopens = capabilities
            .as_ref()
            .unwrap()
            .preopened_paths
            .as_ref()
            .unwrap();
        assert_eq!(preopens.len(), 1);
        assert_eq!(config["path"], "/data/first.db");

        let err = generator
            .apply_host_file_hint("path", &hint, &second_db, &mut config, &mut capabilities)
            .unwrap_err();

        assert!(
            err.to_string()
                .contains("conflicts with an existing preopen")
        );
        assert_eq!(config["path"], "/data/first.db");
    }

    #[test]
    fn mount_file_includes_generated_config_and_capabilities() {
        let manifest = provider_manifest();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("db.json");

        MountFile::new(
            &MountName::try_from("db").unwrap(),
            &manifest,
            None,
            &[],
            GeneratedMountConfig {
                config: Some(serde_json::json!({"path": "/data/chinook.db"})),
                capabilities: Some(ProviderCapabilities {
                    preopened_paths: Some(vec![PreopenedPath {
                        host: "/host/db".to_string(),
                        guest: "/data".to_string(),
                        mode: PreopenMode::Ro,
                    }]),
                    ..ProviderCapabilities::default()
                }),
            },
        )
        .write_to(&out)
        .unwrap();

        let written: Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();

        assert_eq!(written["config"]["path"], "/data/chinook.db");
        assert_eq!(
            written["capabilities"]["preopened_paths"][0]["host"],
            "/host/db"
        );
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
                AuthScheme::StaticToken(omnifs_provider::StaticTokenScheme {
                    key: "pat".to_string(),
                    header_name: Some("Authorization".to_string()),
                    value_prefix: String::new(),
                    description: "Linear API key".to_string(),
                    inject_domains: vec![],
                    creation_url: None,
                    validation: None,
                }),
                AuthScheme::Oauth(omnifs_provider::OauthScheme {
                    key: "oauth".to_string(),
                    display_name: "Linear OAuth".to_string(),
                    authorization_endpoint: "https://example.com/authorize".to_string(),
                    token_endpoint: "https://example.com/token".to_string(),
                    revocation_endpoint: None,
                    default_client_id: None,
                    default_scopes: vec![],
                    flow: omnifs_provider::OAuthFlow::PkceLoopback(
                        omnifs_provider::PkceLoopbackConfig {
                            redirect_uri_template: "http://127.0.0.1:{port}/cb".to_string(),
                        },
                    ),
                    token_endpoint_auth: omnifs_provider::TokenEndpointAuthMethod::None,
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
            auth_type: omnifs_core::AuthKind::OAuth,
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
        assert_eq!(promoted.auth_type, omnifs_core::AuthKind::StaticToken);
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

    #[test]
    fn load_provider_templates_reads_metadata_from_wasm() {
        let dir = tempfile::tempdir().unwrap();
        let mut manifest = provider_manifest();
        manifest.default_mount = "linear-dev".to_owned();
        std::fs::write(
            dir.path().join("omnifs_provider_linear.wasm"),
            wasm_with_custom_section(
                omnifs_provider::PROVIDER_METADATA_SECTION_NAME,
                &serde_json::to_vec(&manifest).unwrap(),
            ),
        )
        .unwrap();
        std::fs::write(dir.path().join("ignored.wasm"), b"\0asm\x01\0\0\0").unwrap();

        let templates = ProviderCatalog::for_dirs(dir.path().join("mounts"), dir.path())
            .provider_templates()
            .unwrap();

        assert!(templates.contains_key("github"));
        assert_eq!(templates["linear"].manifest.default_mount, "linear-dev");
        assert_eq!(
            templates["linear"].source,
            ProviderSource::Disk(dir.path().join("omnifs_provider_linear.wasm"))
        );
    }

    #[test]
    fn load_provider_templates_includes_builtins_without_provider_dir() {
        let dir = tempfile::tempdir().unwrap();
        let templates =
            ProviderCatalog::for_dirs(dir.path().join("mounts"), dir.path().join("missing"))
                .provider_templates()
                .unwrap();

        assert_eq!(
            templates["github"].manifest.provider,
            "omnifs_provider_github.wasm"
        );
        assert_eq!(
            templates["linear"].manifest.provider,
            "omnifs_provider_linear.wasm"
        );
        assert_eq!(templates["github"].source, ProviderSource::Builtin);
    }

    #[test]
    fn load_provider_templates_prefers_disk_metadata_over_builtin_by_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut manifest = provider_manifest();
        manifest.id = "github".to_string();
        manifest.display_name = "GitHub dev".to_string();
        manifest.provider = "omnifs_provider_github.wasm".to_string();
        manifest.default_mount = "github-dev".to_string();
        std::fs::write(
            dir.path().join("omnifs_provider_github.wasm"),
            wasm_with_custom_section(
                omnifs_provider::PROVIDER_METADATA_SECTION_NAME,
                &serde_json::to_vec(&manifest).unwrap(),
            ),
        )
        .unwrap();

        let templates = ProviderCatalog::for_dirs(dir.path().join("mounts"), dir.path())
            .provider_templates()
            .unwrap();

        assert_eq!(templates["github"].manifest.default_mount, "github-dev");
        assert_eq!(
            templates["github"].source,
            ProviderSource::Disk(dir.path().join("omnifs_provider_github.wasm"))
        );
    }

    fn provider_manifest() -> ProviderManifest {
        use omnifs_provider::{
            AuthInject, AuthScheme, OAuthFlow, OauthScheme, PkceLoopbackConfig,
            ProviderAuthManifest, StaticTokenScheme, TokenEndpointAuthMethod,
        };
        use std::collections::BTreeMap;

        let inject = AuthInject {
            domains: vec!["api.linear.app".to_string()],
            header: "Authorization".to_string(),
            prefix: String::new(),
        };
        ProviderManifest {
            id: "linear".to_string(),
            display_name: "Linear".to_string(),
            provider: "omnifs_provider_linear.wasm".to_string(),
            default_mount: "linear".to_string(),
            capabilities: vec![
                omnifs_provider::CapabilityEntry::Domain {
                    value: "api.linear.app".to_string(),
                    why: "api calls".to_string(),
                    dynamic: false,
                },
                omnifs_provider::CapabilityEntry::MemoryMb {
                    value: 128,
                    why: "in-memory caching".to_string(),
                    dynamic: false,
                },
            ],
            auth: Some(ProviderAuthManifest {
                inject: inject.clone(),
                default: "oauth".to_string(),
                schemes: {
                    let mut m = BTreeMap::new();
                    m.insert(
                        "oauth".to_string(),
                        AuthScheme::Oauth(OauthScheme {
                            key: "oauth".to_string(),
                            display_name: "Linear OAuth".to_string(),
                            authorization_endpoint: "https://linear.app/oauth/authorize"
                                .to_string(),
                            token_endpoint: "https://api.linear.app/oauth/token".to_string(),
                            revocation_endpoint: None,
                            default_client_id: Some("test-client-id".to_string()),
                            default_scopes: vec!["read".to_string()],
                            flow: OAuthFlow::PkceLoopback(PkceLoopbackConfig {
                                redirect_uri_template: "http://127.0.0.1:{port}/callback"
                                    .to_string(),
                            }),
                            token_endpoint_auth: TokenEndpointAuthMethod::None,
                            refresh_token_rotates: true,
                            extra_authorize_params: vec![],
                            extra_token_params: vec![],
                            inject_domains: inject.domains.clone(),
                            inject_header_name: Some(inject.header.clone()),
                            inject_value_prefix: inject.prefix.clone(),
                        }),
                    );
                    m.insert(
                        "pat".to_string(),
                        AuthScheme::StaticToken(StaticTokenScheme {
                            key: "pat".to_string(),
                            header_name: Some(inject.header.clone()),
                            value_prefix: inject.prefix.clone(),
                            description: "Linear API key".to_string(),
                            inject_domains: inject.domains.clone(),
                            creation_url: None,
                            validation: None,
                        }),
                    );
                    m
                },
            }),
            config_schema: None,
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
