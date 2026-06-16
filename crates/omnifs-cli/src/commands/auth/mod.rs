//! `omnifs auth` provider-agnostic credential commands.

mod import;
mod login;
mod logout;
mod shared;
mod status;

use clap::{Args, Subcommand};
use omnifs_creds::FileStore;
use std::path::PathBuf;

use crate::app_context::AppContext;
use crate::paths::PathOverrides;
use crate::presentation::OutputFormat;

pub(crate) use import::run_auth_manifest;
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
        let ctx = AppContext::resolve(PathOverrides::default(), None, None)?;
        let mut paths = ctx.paths().clone();
        if let Some(creds) = self.credentials_file.clone() {
            paths.credentials_file = creds;
        }
        let catalog = ctx.catalog();
        let mounts = ctx.workspace().mounts()?;
        let store = Box::new(FileStore::new(&paths.credentials_file));
        match self.command {
            // A static reference card; ignores the mount/credential context above.
            AuthCommand::Modes => {
                crate::auth::explain::render_modes_catalog();
                Ok(())
            },
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

#[cfg(test)]
mod tests;
