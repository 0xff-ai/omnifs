//! `omnifs mount add` — interactive creation of a new mount.
//!
//! Walks the user through naming a mount, discovers provider defaults from
//! the built-in catalog or provider wasm metadata, writes the resulting mount config to
//! the resolved omnifs config file, and runs the provider's default auth flow
//! when one is declared.

use anyhow::Context;
use clap::Args;
use omnifs_workspace::authn::{AccountId, CredentialId};
use omnifs_workspace::creds::{CredentialEntry, CredentialStore};
use omnifs_workspace::mounts::Auth;
use omnifs_workspace::provider::ProviderManifest;
use secrecy::{ExposeSecret, SecretString};
use time::OffsetDateTime;

use super::token_validation::validate_static_token;
use omnifs_workspace::Workspace;

#[derive(Args, Debug, Clone)]
#[command(after_help = "Examples:\n  omnifs mount add\n  omnifs mount add github --name work")]
pub struct AddArgs {
    /// Provider to use (positional; picker if omitted).
    pub provider: Option<String>,
    /// Mount name override. Auto-generated from the provider if absent.
    #[arg(long)]
    pub name: Option<String>,
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
    /// Full resource limits JSON object to write into the mount spec.
    #[arg(long = "limits-json", value_name = "JSON")]
    pub limits_json: Option<String>,
}

impl AddArgs {
    pub async fn run(
        self,
        output: crate::ui::output::Output,
    ) -> anyhow::Result<crate::error::ExitCode> {
        let workspace = Workspace::resolve()?;
        self.run_in_workspace(&workspace, output).await
    }

    pub(crate) async fn run_in_workspace(
        self,
        workspace: &Workspace,
        output: crate::ui::output::Output,
    ) -> anyhow::Result<crate::error::ExitCode> {
        let prompt = crate::stages::PromptMode::from_flags(
            output.yes(),
            output.no_input() || output.is_structured(),
        );
        let outcome = crate::stages::configure_mount(
            self,
            workspace,
            &output,
            prompt,
            crate::stages::ReceiptStyle::Full,
        )
        .await?;
        match outcome.status {
            crate::stages::MountInitStatus::Ready => {
                // `mount add` only ever commits a new desired revision; a
                // daemon (if one happens to be running) still serves whatever
                // revision it applied at its own last start, so the mount
                // just added is never already serving. The `omnifs up` hint
                // is the only honest closing line here, exactly once,
                // regardless of daemon liveness.
                output.outro(format!(
                    "Mounted `{0}` at /{0}. Serve it: `omnifs up`",
                    outcome.mount_name
                ));
            },
            crate::stages::MountInitStatus::SignInDeclined => {
                output.outro(format!(
                    "Saved `{}`. Run `omnifs mount reauth {}` to sign in later.",
                    outcome.mount_name, outcome.mount_name
                ));
            },
        }
        if output.is_structured() {
            output.emit_result(
                crate::ui::output::ResultVerdict::Ok,
                &crate::commands::receipt::MountAddReceipt {
                    verdict: crate::commands::receipt::Verdict::Ok,
                    mount: outcome.mount_name,
                    status: outcome.status,
                },
            )?;
        }
        Ok(crate::error::ExitCode::Success)
    }
}

/// The per-provider consent block for `mount add`: a plain description line,
/// then compact needs and limits lines.
/// All on stderr.
pub(crate) fn render_consent_block(
    output: &crate::ui::output::Output,
    manifest: &ProviderManifest,
) {
    let description = manifest
        .description
        .as_deref()
        .unwrap_or(&manifest.display_name);
    output.narrate(description);
    if let Some(needs) = crate::capability::compact_needs(manifest) {
        output.narrate(crate::ui::style::dim(
            needs,
            crate::ui::style::Stream::Stderr,
        ));
    }
    if let Some(limits) = crate::capability::compact_limits(manifest) {
        output.narrate(crate::ui::style::dim(
            limits,
            crate::ui::style::Stream::Stderr,
        ));
    }
}

pub(crate) async fn run_static_token_init(
    manifest: &ProviderManifest,
    auth: &Auth,
    token: SecretString,
    store: &dyn CredentialStore,
    validate: bool,
    output: &crate::ui::output::Output,
    key_width: usize,
) -> anyhow::Result<CredentialId> {
    let static_token_scheme = crate::auth::static_token_scheme(auth, manifest)?;

    let header_name = static_token_scheme
        .header_name
        .as_deref()
        .unwrap_or("Authorization");
    let header_prefix = static_token_scheme.value_prefix.as_str();

    let validation = match static_token_scheme.validation.as_ref() {
        Some(v) if validate => Some(
            validate_static_token(v, header_name, header_prefix, token.expose_secret(), output)
                .await?,
        ),
        Some(_) => {
            output.narrate("token stored without validation (--no-validate)");
            None
        },
        None => None,
    };
    let identity = validation
        .as_ref()
        .and_then(|outcome| outcome.identity.clone());
    output.ledger_row(
        &crate::ui::render::LedgerRow::new(
            crate::ui::style::Glyph::Done,
            "signed in",
            identity
                .clone()
                .unwrap_or_else(|| "token accepted".to_string()),
        ),
        key_width,
    );
    if let Some(outcome) = &validation
        && let Some(workspace) = &outcome.workspace
    {
        output.narrate(workspace);
    }

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
        .static_token_scheme_key(auth.scheme(), None)?;
    let account = auth
        .account()
        .map_or_else(|| AccountId::default_account().to_string(), str::to_owned);
    let credential_id = CredentialId::new(&manifest.id, &scheme_key, account)?;
    store
        .put(&credential_id, &entry)
        .with_context(|| "failed to store credential")?;
    output.ledger_row(
        &crate::ui::render::LedgerRow::new(crate::ui::style::Glyph::Done, "credential", "stored"),
        key_width,
    );
    Ok(credential_id)
}

#[cfg(test)]
mod tests {
    use super::AddArgs;
    use crate::commands::mount::AuthImportDecision;
    use crate::commands::mount::spec_creation::{create_config, validate_config};
    use omnifs_workspace::Workspace;
    use omnifs_workspace::authn::{AuthManifest, AuthScheme};
    use omnifs_workspace::provider::{
        AccessNeed, ConfigField, ConfigMetadata, ConfigType, HostResourceBinding,
        LimitDeclarations, PreopenMode, ProviderManifest, ResourceLimit,
    };

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

        let output = crate::ui::output::Output::new(crate::ui::output::OutputMode::Human, false);

        create_config(&manifest, &output, false)
            .expect_err("default generation must fail without the required field");

        validate_config(&manifest, &serde_json::json!({"path": "/data/test.db"}))
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

        let output = crate::ui::output::Output::new(crate::ui::output::OutputMode::Human, false);
        let config = create_config(&manifest, &output, false).unwrap();

        assert_eq!(
            config,
            Some(serde_json::json!({"endpoint": "unix:///var/run/docker.sock"})),
        );
    }

    #[test]
    fn provider_default_auth_uses_the_declared_scheme() {
        let selection = crate::auth::auth_from_provider_default(&provider_manifest()).unwrap();
        assert!(selection.is_oauth());
        assert_eq!(selection.scheme(), Some("oauth"));
        assert_eq!(selection.account(), None);
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

        assert!(manifest.requires_mount_input());
    }

    #[test]
    fn add_dns_writes_snapshot_spec() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::under_root(dir.path());
        let args = AddArgs {
            provider: Some("dns".to_string()),
            name: None,
            no_browser: true,
            token: None,
            token_env: None,
            no_validate: false,
            scopes: Vec::new(),
            scheme: None,
            no_auth: false,
            config_json: None,
            limits_json: None,
        };

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(args.run_in_workspace(
                &workspace,
                crate::ui::output::Output::new(crate::ui::output::OutputMode::Human, false),
            ))
            .unwrap();

        let spec = std::fs::read_to_string(dir.path().join("mounts/dns.json")).unwrap();
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
                    ambient_sources: vec![omnifs_workspace::authn::AmbientSource {
                        kind: omnifs_workspace::authn::AmbientKind::EnvVar {
                            name: "LINEAR_API_KEY".into(),
                        },
                        note: String::new(),
                    }],
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
        let oauth_default =
            omnifs_workspace::mounts::Auth::OAuth(omnifs_workspace::mounts::OAuth {
                scheme: Some("oauth".to_string()),
                ..Default::default()
            });

        let outcome = AuthImportDecision::new(
            Some(oauth_default),
            Some(&auth_manifest),
            "linear",
            true,
            true,
        )
        .resolve(
            &crate::ui::output::Output::new(crate::ui::output::OutputMode::Human, false),
            crate::auth::auth_receipt_key_width(),
        )
        .unwrap();

        let promoted = outcome.auth.expect("auth");
        assert_eq!(
            promoted.kind(),
            omnifs_workspace::authn::AuthKind::StaticToken
        );
        assert_eq!(promoted.scheme(), Some("pat"));
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
                    ambient_sources: vec![omnifs_workspace::authn::AmbientSource {
                        kind: omnifs_workspace::authn::AmbientKind::EnvVar {
                            name: "LINEAR_API_KEY".into(),
                        },
                        note: String::new(),
                    }],
                },
            )],
        };
        let static_default =
            omnifs_workspace::mounts::Auth::StaticToken(omnifs_workspace::mounts::StaticToken {
                scheme: Some("pat".to_string()),
                account: None,
            });

        let outcome = AuthImportDecision::new(
            Some(static_default),
            Some(&auth_manifest),
            "linear",
            false, // non-interactive
            true,  // --yes
        )
        .resolve(
            &crate::ui::output::Output::new(crate::ui::output::OutputMode::Human, false),
            crate::auth::auth_receipt_key_width(),
        )
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

    /// A web-like provider: a *dynamic* domain need plus a `domains` string
    /// array config field with no default and no auth. This is the shape that
    /// leaves the authority for the mount to supply.
    fn web_manifest() -> ProviderManifest {
        let mut manifest = provider_manifest();
        manifest.id = "web".to_string();
        manifest.display_name = "Web".to_string();
        manifest.default_mount = "web".to_string();
        manifest.auth = None;
        manifest.capabilities = vec![AccessNeed::Domain {
            value: "resolved from config at mount-start".to_string(),
            why: "fetch configured domains".to_string(),
            dynamic: true,
        }];
        manifest.config = Some(ConfigMetadata {
            fields: vec![ConfigField {
                name: "domains".to_string(),
                value_type: ConfigType::Array {
                    items: Box::new(ConfigType::String),
                },
                required: false,
                default: None,
                description: None,
                binding: None,
            }],
        });
        manifest
    }

    #[test]
    fn dynamic_domain_provider_requires_domains_input() {
        // The web provider declares a dynamic domain need and reads its
        // authority from a `domains` config field with no default. The flow
        // must treat a dynamic-domain provider as needing input: a
        // non-interactive run without --config-json bails asking for it rather
        // than writing a spec that can never be served.
        let manifest = web_manifest();
        assert!(
            manifest.requires_mount_input(),
            "a dynamic-domain provider must require domain input"
        );
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
            refresh_interval_secs: 0,
            capabilities: vec![AccessNeed::Domain {
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
}
