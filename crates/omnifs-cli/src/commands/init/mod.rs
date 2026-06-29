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
use omnifs_creds::{CredentialEntry, CredentialStore, FileStore};
use omnifs_provider::ProviderManifest;
use secrecy::{ExposeSecret, SecretString};
use std::path::Path;
use time::OffsetDateTime;

use crate::auth::AuthSelection;
use crate::credential_target::CredentialTarget;
use crate::launch_backend::LaunchBackend;
use crate::token_source::TokenSource;
use crate::workspace::Workspace;
pub(crate) use auth_import::AuthImportDecision;
use mount_file::MountFile;
use omnifs_home::WorkspaceLayout;
use omnifs_mount::mounts::Registry;
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
    /// Re-authenticate an existing mount instead of creating one. The positional
    /// argument names the mount to re-authenticate.
    #[arg(long)]
    pub reauth: bool,
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
    /// Internal: setup prints capability grants before its own confirmation.
    #[arg(skip = true)]
    pub(crate) show_capabilities: bool,
}

impl InitArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        let workspace = Workspace::resolve()?;
        self.run_in_workspace(&workspace).await
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) async fn run_in_workspace(self, workspace: &Workspace) -> anyhow::Result<()> {
        if self.reauth {
            return self.run_reauth(workspace).await;
        }
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
        let auth_manifest = manifest.wasm_auth_manifest();
        let default_auth = AuthSelection::from_provider_default(manifest);
        if interactive && self.show_capabilities {
            print_capability_justifications(manifest);
        }
        if self.no_input && default_auth.as_ref().is_some_and(AuthSelection::is_oauth) {
            anyhow::bail!(
                "`omnifs init --no-input` cannot complete OAuth. Run `omnifs init {provider_name}` interactively."
            );
        }
        let creator = MountSpecCreator::new(manifest);
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
        Registry::load(&paths.mounts_dir)?.put(&spec)?;
        anstream::println!("✓ Wrote {}", WorkspaceLayout::display(&mount_path));

        if let Some(auth) = effective_auth.as_ref() {
            if let Some(token) = import_outcome.token {
                run_static_token_init(manifest, auth, token, &paths.credentials_file).await?;
            } else if auth.is_oauth() {
                anstream::println!("Starting OAuth login for `{mount_name}` ...");
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
                        "Mount `{mount_name}` was created, but login did not complete. Run `omnifs init --reauth {mount_name}` to finish."
                    );
                })?;
            } else {
                if interactive && let Ok(scheme) = auth.static_token_scheme(manifest) {
                    let guidance = manifest
                        .auth
                        .as_ref()
                        .map(|auth| auth.guidance_for(&scheme.key))
                        .unwrap_or_default();
                    anstream::println!();
                    anstream::println!("Authenticating `{mount_name}` with a static token:");
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

        anstream::println!();
        anstream::println!("Mount `{mount_name}` is ready.");

        let config = crate::session::MountConfig::from_parsed(spec, mount_path.clone())?;
        let store = FileStore::new(&paths.credentials_file);
        let backend = LaunchBackend::resolve(&workspace.config()?, None, None)?;
        match crate::live::add_mount(workspace.daemon(), catalog, &store, config, &backend).await {
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

    /// Re-acquire the credential for an existing mount: OAuth login or a fresh
    /// static token, dispatched on the mount's stored auth. The spec is left
    /// untouched; only the credential store changes.
    async fn run_reauth(self, workspace: &Workspace) -> anyhow::Result<()> {
        let paths = workspace.layout();
        let mount_name = self.provider.as_deref().ok_or_else(|| {
            anyhow!("name the mount to re-authenticate: `omnifs init --reauth <mount>`")
        })?;
        let mounts = workspace.mounts()?;
        let mount_config = mounts
            .iter()
            .find(|m| m.name.as_str() == mount_name)
            .ok_or_else(|| {
                anyhow!("no mount named `{mount_name}`; run `omnifs init <provider>` to create it")
            })?;
        let Some(auth) = mount_config.config.auth.as_ref() else {
            anyhow::bail!("mount `{mount_name}` needs no authentication");
        };

        let installed = crate::catalog::installed_providers(workspace.catalog())?;
        let provider_name = mount_config.config.provider_name();
        let (_, manifest) = crate::catalog::find_installed(&installed, provider_name.as_str())
            .ok_or_else(|| {
                anyhow!("provider `{provider_name}` for mount `{mount_name}` is not installed")
            })?;

        let selection = AuthSelection {
            auth_type: auth.kind(),
            scheme: auth.scheme().map(str::to_owned),
            account: auth.account().map(str::to_owned),
        };

        if selection.is_oauth() {
            anstream::println!("Re-authenticating `{mount_name}` over OAuth ...");
            crate::auth::login_with_workspace(
                workspace,
                mount_name,
                selection.account.as_deref(),
                self.no_browser,
                &self.scopes,
            )
            .await?;
        } else {
            let source = TokenSource::resolve(
                self.token.as_deref(),
                self.token_env.as_deref(),
                !self.no_input,
            )?;
            let token = source.read()?;
            run_static_token_init(manifest, &selection, token, &paths.credentials_file).await?;
        }

        anstream::println!();
        anstream::println!("✓ Re-authenticated `{mount_name}`.");
        anstream::println!(
            "If a daemon is running, restart it with `omnifs up` to apply the new credential."
        );
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

pub(crate) async fn run_static_token_init(
    manifest: &ProviderManifest,
    auth: &AuthSelection,
    token: SecretString,
    credentials_file: &Path,
) -> anyhow::Result<()> {
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
            anstream::println!("✓ Authenticated as {identity}");
        } else {
            anstream::println!("✓ Token accepted");
        }
        if let Some(workspace) = &outcome.workspace {
            anstream::println!("✓ Workspace: {workspace}");
        }
    }

    let store = FileStore::new(credentials_file);
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
    use super::spec_creation::{CreatedMountSpec, MountSpecCreator};
    use super::{AuthImportDecision, MountFile};
    use crate::auth::AuthSelection;
    use omnifs_caps::{Grant, Grants as ProviderCapabilities, PreopenMode, PreopenedPath};
    use omnifs_core::{MountName, ProviderId, ProviderMeta, ProviderName, ProviderRef};
    use omnifs_mount::mounts::Registry;
    use omnifs_provider::{
        AuthManifest, AuthScheme, Catalog, ConfigField, ConfigMetadata, ConfigType,
        HostResourceBinding, ProviderManifest, ProviderStore,
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

        let created = MountSpecCreator::new(&manifest).create(false).unwrap();

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
        assert_eq!(capabilities.max_memory_mb, Some(128));
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

        assert!(MountSpecCreator::new(&manifest).requires_prompt());
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
        let paths = omnifs_home::WorkspaceLayout::under_root(dir.path());

        let mut manifest = provider_manifest();
        manifest.default_mount = "linear-dev".to_owned();
        let wasm = wasm_with_custom_section(
            omnifs_provider::PROVIDER_METADATA_SECTION_NAME,
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

    /// An empty store yields no templates: there is no built-in fallback set.
    #[test]
    fn installed_providers_empty_without_installed_providers() {
        let dir = tempfile::tempdir().unwrap();
        let paths = omnifs_home::WorkspaceLayout::under_root(dir.path());
        let installed =
            crate::catalog::installed_providers(&Catalog::open(&paths.providers_dir)).unwrap();
        assert!(installed.is_empty());
    }

    fn provider_manifest() -> ProviderManifest {
        use omnifs_provider::{
            AuthScheme, OAuthFlow, OauthScheme, PkceLoopbackConfig, ProviderAuthManifest,
            StaticTokenScheme, TokenEndpointAuthMethod,
        };
        use std::collections::BTreeMap;

        let domains = vec!["api.linear.app".to_string()];
        ProviderManifest {
            id: "linear".to_string(),
            display_name: "Linear".to_string(),
            provider: "omnifs_provider_linear.wasm".to_string(),
            default_mount: "linear".to_string(),
            version: None,
            capabilities: vec![
                omnifs_caps::Need::Domain {
                    value: "api.linear.app".to_string(),
                    why: "api calls".to_string(),
                    dynamic: false,
                },
                omnifs_caps::Need::MemoryMb {
                    value: 128,
                    why: "in-memory caching".to_string(),
                    dynamic: false,
                },
            ],
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
