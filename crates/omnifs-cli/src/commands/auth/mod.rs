//! `omnifs auth` provider-agnostic credential commands.

mod import;
mod login;
mod logout;
mod shared;
mod status;

use clap::{Args, Subcommand};
use omnifs_creds::FileStore;
use omnifs_provider::ProviderManifest;
use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::catalog::{ProviderCatalog, ProviderTemplate};
use crate::cli::OutputFormat;
use crate::paths::PathOverrides;
use crate::session::MountConfig;
use crate::workspace::Workspace;

pub(crate) use login::login_with_paths;

#[derive(Debug, Clone, Args)]
pub struct AuthArgs {
    /// Override the credential file path.
    #[arg(long)]
    pub credentials_file: Option<PathBuf>,
    #[command(subcommand)]
    pub command: AuthCommand,
}

#[derive(Debug, Clone, Subcommand)]
pub enum AuthCommand {
    /// List the authentication mechanisms omnifs supports, in general.
    Modes,
    /// Explain how to authenticate a provider or mount, scheme by scheme.
    Explain {
        /// Provider id (e.g. `github`) or a configured mount name.
        target: String,
        /// Print the raw auth manifest as JSON instead of rendered guidance.
        #[arg(long)]
        json: bool,
    },
    Login {
        mount: String,
        #[arg(long)]
        no_browser: bool,
        #[arg(long = "scope")]
        scopes: Vec<String>,
    },
    Logout {
        mount: String,
        #[arg(long)]
        revoke: bool,
    },
    Status {
        #[arg(long)]
        json: bool,
    },
    Refresh {
        mount: String,
    },
    Scopes {
        mount: String,
    },
    Import {
        mount: String,
        #[arg(long, conflicts_with = "token_env")]
        token: Option<String>,
        #[arg(long, value_name = "ENV_VAR", conflicts_with = "token")]
        token_env: Option<String>,
        #[arg(long)]
        scheme: Option<String>,
    },
}

impl AuthArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        let workspace = Workspace::resolve(PathOverrides::default())?;
        let mut paths = workspace.paths().clone();
        if let Some(creds) = self.credentials_file.clone() {
            paths.credentials_file = creds;
        }
        let catalog = workspace.catalog();
        let mounts = workspace.mounts()?;
        let store = Box::new(FileStore::new(&paths.credentials_file));
        match self.command {
            // A static reference card; ignores the mount/credential context above.
            AuthCommand::Modes => {
                crate::auth::explain::render_modes_catalog();
                Ok(())
            },
            AuthCommand::Explain { target, json } => run_explain(catalog, &mounts, &target, json),
            AuthCommand::Login {
                mount,
                no_browser,
                scopes,
            } => login::login(catalog, &mounts, store, &mount, None, no_browser, &scopes).await,
            AuthCommand::Logout { mount, revoke } => {
                logout::logout(
                    &paths,
                    catalog,
                    &mounts,
                    store.as_ref(),
                    &mount,
                    None,
                    revoke,
                )
                .await
            },
            AuthCommand::Status { json } => match OutputFormat::from(json) {
                OutputFormat::Json => status::status_json(&paths, catalog, mounts, store.as_ref()),
                OutputFormat::Text => status::status(&paths, catalog, mounts, store.as_ref()),
            },
            AuthCommand::Refresh { mount } => {
                import::refresh(&paths, catalog, &mounts, store, &mount, None).await
            },
            AuthCommand::Scopes { mount } => {
                import::scopes(&paths, catalog, &mounts, store.as_ref(), &mount, None)
            },
            AuthCommand::Import {
                mount,
                token,
                token_env,
                scheme,
            } => {
                let source = crate::token_source::TokenSource::resolve(
                    token.as_deref(),
                    token_env.as_deref(),
                    false,
                )?;
                let token = source.read()?;
                import::import_static_token_value(
                    catalog,
                    &mounts,
                    store.as_ref(),
                    &mount,
                    token,
                    scheme.as_deref(),
                    None,
                )
            },
        }
    }
}

fn run_explain(
    catalog: &ProviderCatalog,
    mounts: &[MountConfig],
    target: &str,
    json: bool,
) -> anyhow::Result<()> {
    let templates = catalog.provider_templates()?;
    let manifest = resolve_target_manifest(&templates, mounts, target)?;

    if json {
        match manifest.wasm_auth_manifest() {
            Some(wire) => anstream::println!("{}", serde_json::to_string_pretty(&wire)?),
            None => anstream::println!("null"),
        }
        return Ok(());
    }

    match &manifest.auth {
        Some(auth) => crate::auth::explain::render_provider_auth(&manifest.display_name, auth),
        None => anstream::println!("{} needs no authentication.", manifest.display_name),
    }
    Ok(())
}

/// Resolve an `auth explain` target, which may be a provider id or a configured
/// mount name, to the owning provider manifest.
fn resolve_target_manifest<'a>(
    templates: &'a BTreeMap<String, ProviderTemplate>,
    mounts: &[MountConfig],
    target: &str,
) -> anyhow::Result<&'a ProviderManifest> {
    if let Some(template) = templates.get(target) {
        return Ok(&template.manifest);
    }
    if let Some(mount) = mounts.iter().find(|m| m.name.as_str() == target) {
        let provider_ref = &mount.config.provider;
        if let Some(template) = templates
            .values()
            .find(|t| t.manifest.id == *provider_ref || t.manifest.provider == *provider_ref)
        {
            return Ok(&template.manifest);
        }
    }
    anyhow::bail!(
        "no provider or mount named `{target}`; known providers: {}",
        templates.keys().cloned().collect::<Vec<_>>().join(", ")
    )
}

#[cfg(test)]
mod tests;
