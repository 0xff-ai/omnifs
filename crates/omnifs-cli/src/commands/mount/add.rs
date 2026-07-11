//! `omnifs mount add` — interactive setup for a new mount.
//!
//! Walks the user through naming a mount, discovers provider defaults from
//! the built-in catalog or provider wasm metadata, writes the resulting mount config to
//! the resolved omnifs config file, and runs the provider's default auth flow
//! when one is declared.

use anyhow::Context;
use clap::Args;
use omnifs_workspace::creds::{CredentialEntry, CredentialStore, FileStore};
use omnifs_workspace::provider::ProviderManifest;
use secrecy::{ExposeSecret, SecretString};
use std::path::Path;
use time::OffsetDateTime;

use super::token_validation::StaticTokenValidator;
use crate::auth::AuthSelection;
use crate::credential_target::CredentialTarget;
use crate::workspace::Workspace;

#[derive(Args, Debug, Clone)]
#[allow(clippy::struct_excessive_bools)] // mirrors CLI flags 1:1
pub struct AddArgs {
    /// Provider to use (positional; picker if omitted).
    pub provider: Option<String>,
    /// Mount name override. Auto-generated from the provider if absent.
    #[arg(long = "as")]
    pub as_name: Option<String>,
    /// Skip prompts. Static-token providers also require --token or --token-env.
    #[arg(long)]
    pub no_input: bool,
    /// Accept the suggested mount name on a collision (never overwrites), and
    /// accept a detected ambient credential without prompting.
    #[arg(short = 'y', long)]
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
    /// Store the static token without the provider's upstream validation
    /// probe (for CI or restricted tokens that fail the probe endpoint but
    /// work for their intended scope).
    #[arg(long)]
    pub no_validate: bool,
    /// OAuth scope to request. Repeat for multiple scopes.
    #[arg(long = "scope")]
    pub scopes: Vec<String>,
    /// Auth scheme to use instead of the provider default.
    #[arg(long, value_name = "SCHEME")]
    pub scheme: Option<String>,
    /// Do not write an auth block, even if the provider declares a default.
    #[arg(long, conflicts_with_all = ["token", "token_env", "scheme"])]
    pub no_auth: bool,
    /// Full provider config JSON object to write into the mount spec.
    #[arg(long = "config-json", value_name = "JSON")]
    pub config_json: Option<String>,
    /// Full capability grants JSON object to write into the mount spec.
    #[arg(long = "capabilities-json", value_name = "JSON")]
    pub capabilities_json: Option<String>,
    /// Full resource limits JSON object to write into the mount spec.
    #[arg(long = "limits-json", value_name = "JSON")]
    pub limits_json: Option<String>,
}

impl AddArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        let workspace = Workspace::resolve()?;
        self.run_in_workspace(&workspace).await
    }

    pub(crate) async fn run_in_workspace(self, workspace: &Workspace) -> anyhow::Result<()> {
        let mut session = crate::ui::session::Session::intro("omnifs mount add")?;
        let outcome = crate::stages::configure_mount(self, workspace, true, &mut session).await?;
        match outcome.status {
            crate::stages::MountInitStatus::Ready => {
                session.outro(format!("Mounted `{}`.", outcome.mount_name));
            },
            crate::stages::MountInitStatus::SignInDeclined => {
                session.outro(format!(
                    "Saved `{}`. Run `omnifs mount reauth {}` to sign in later.",
                    outcome.mount_name, outcome.mount_name
                ));
            },
        }
        Ok(())
    }
}

/// The per-provider consent block shared by setup's loop and standalone
/// `mount add`: a plain description line, then compact needs and limits lines.
/// All on stderr.
pub(crate) fn render_consent_block(
    session: &mut crate::ui::session::Session,
    manifest: &ProviderManifest,
) {
    let description = manifest
        .description
        .as_deref()
        .unwrap_or(&manifest.display_name);
    session.note(description);
    if let Some(needs) = crate::capability::compact_needs(manifest) {
        session.note(crate::style::dim(needs));
    }
    if let Some(limits) = crate::capability::compact_limits(manifest) {
        session.note(crate::style::dim(limits));
    }
}

pub(crate) async fn run_static_token_init(
    manifest: &ProviderManifest,
    auth: &AuthSelection,
    token: SecretString,
    credentials_file: &Path,
    validate: bool,
    session: &mut crate::ui::session::Session,
) -> anyhow::Result<CredentialTarget> {
    let static_token_scheme = auth.static_token_scheme(manifest)?;

    let header_name = static_token_scheme
        .header_name
        .as_deref()
        .unwrap_or("Authorization");
    let header_prefix = static_token_scheme.value_prefix.as_str();

    let validation = match static_token_scheme.validation.as_ref() {
        Some(v) if validate => Some(
            StaticTokenValidator::new(v, header_name, header_prefix)
                .validate(token.expose_secret(), session)
                .await?,
        ),
        Some(_) => {
            session.note("token stored without validation (--no-validate)");
            None
        },
        None => None,
    };
    let identity = validation
        .as_ref()
        .and_then(|outcome| outcome.identity.clone());
    session.row(crate::ui::report::Row::new(
        crate::ui::style::Glyph::Done,
        "signed in",
        identity
            .clone()
            .unwrap_or_else(|| "token accepted".to_string()),
    ));
    if let Some(outcome) = &validation
        && let Some(workspace) = &outcome.workspace
    {
        session.note(workspace);
    }

    let store = FileStore::new(credentials_file);
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
    session.row(crate::ui::report::Row::new(
        crate::ui::style::Glyph::Done,
        "credential",
        "stored",
    ));
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::AddArgs;
    use crate::auth::AuthSelection;
    use crate::commands::mount::AuthImportDecision;
    use crate::commands::mount::mount_file::MountFile;
    use crate::commands::mount::spec_creation::{CreatedMountSpec, MountSpecCreator};
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
    fn config_override_skips_default_generation_for_required_fields() {
        // A provider whose config has a required field with no default (db's
        // `path`) cannot generate a valid default config; a supplied
        // --config-json must bypass default generation, not fail on it.
        let mut manifest = provider_manifest();
        manifest.config = Some(ConfigMetadata {
            fields: vec![ConfigField {
                name: "path".to_string(),
                value_type: ConfigType::String,
                required: true,
                default: None,
                description: None,
                binding: None,
            }],
        });

        let reference = provider_ref("db");
        let mount_name = MountName::try_from("db").unwrap();
        let creator = MountSpecCreator::new(&reference, &mount_name, &manifest);

        creator
            .create(false)
            .expect_err("default generation must fail without the required field");

        let created = creator.create_for_config_override();
        assert_eq!(created.config, None);
        creator
            .validate(&serde_json::json!({"path": "/data/test.db"}))
            .expect("override config with the required field validates");
    }

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
        // `mount add` seeds the spec's grants from the manifest's needs, so a
        // mount carries explicit grants the materialize-time check can satisfy.
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
    fn add_dns_writes_snapshot_spec() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::from_layout(
            omnifs_workspace::layout::WorkspaceLayout::under_root(dir.path()),
        );
        let args = AddArgs {
            provider: Some("dns".to_string()),
            as_name: None,
            no_input: true,
            yes: true,
            no_browser: true,
            token: None,
            token_env: None,
            no_validate: false,
            scopes: Vec::new(),
            scheme: None,
            no_auth: false,
            config_json: None,
            capabilities_json: None,
            limits_json: None,
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
        .resolve(None)
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

    #[test]
    #[allow(unsafe_code)] // env::set_var/remove_var require unsafe; guarded by lock_env().
    fn import_outcome_accepts_ambient_credential_non_interactively_with_yes() {
        // `--yes` must accept a detected ambient credential even when
        // interactive=false, so the documented scripted behavior is reachable.
        let _guard = lock_env();
        // SAFETY: env mutation is isolated by the lock_env() guard above.
        unsafe {
            std::env::set_var("LINEAR_API_KEY", "lin_api_xxx");
        }
        let auth_manifest = AuthManifest {
            schemes: vec![AuthScheme::StaticToken(
                omnifs_workspace::authn::StaticTokenScheme {
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
                },
            )],
        };
        let static_default = AuthSelection {
            auth_type: omnifs_workspace::authn::AuthKind::StaticToken,
            scheme: Some("pat".to_string()),
            account: None,
        };

        let outcome = AuthImportDecision::new(
            Some(static_default),
            Some(&auth_manifest),
            "linear",
            false, // non-interactive
            true,  // --yes
        )
        .resolve(None)
        .unwrap();

        assert!(
            outcome.token.is_some(),
            "non-interactive --yes must import the ambient credential"
        );

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
            description: None,
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
