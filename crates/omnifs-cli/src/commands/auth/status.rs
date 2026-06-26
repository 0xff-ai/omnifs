//! Auth status reporting.

use std::fmt::Write as _;

use omnifs_creds::{CredentialStore, Refreshability};

use super::shared::format_scopes;
use crate::auth::explain::AuthMode;
use crate::auth::{AuthReadiness, MountAuth};
use crate::catalog::ProviderCatalog;
use crate::session::MountConfig;
use omnifs_home::WorkspaceLayout;
use omnifs_provider::ProviderAuthManifest;

pub(super) fn status(
    layout: &WorkspaceLayout,
    catalog: &ProviderCatalog,
    mounts: Vec<MountConfig>,
    store: &dyn CredentialStore,
) -> anyhow::Result<()> {
    let rows = load_auth_rows(catalog, store, mounts)?;
    anstream::println!("backend: {}", store.backend_label());
    if rows.is_empty() {
        anstream::println!("no mount configs found in {}", layout.config_file.display());
        return Ok(());
    }
    for row in rows {
        anstream::println!("{}: {}", row.mount, row.text_detail());
        if let Some(line) = row.available_line() {
            anstream::println!("  {line}");
        }
    }
    Ok(())
}

#[derive(serde::Serialize)]
pub(super) struct AuthStatusJson {
    entries: Vec<AuthEntryJson>,
}

#[derive(serde::Serialize)]
struct AuthEntryJson {
    key: String,
    auth: AuthReadiness,
    available_schemes: Vec<String>,
}

pub(super) fn status_json(
    catalog: &ProviderCatalog,
    mounts: Vec<MountConfig>,
    store: &dyn CredentialStore,
) -> anyhow::Result<()> {
    let entries = load_auth_rows(catalog, store, mounts)?
        .into_iter()
        .map(AuthStatusRow::into_json)
        .collect();
    let payload = AuthStatusJson { entries };
    anstream::println!("{}", serde_json::to_string(&payload)?);
    Ok(())
}

fn load_auth_rows(
    catalog: &ProviderCatalog,
    store: &dyn CredentialStore,
    mounts: Vec<MountConfig>,
) -> anyhow::Result<Vec<AuthStatusRow>> {
    let auth_mounts = catalog.load_all_mount_auth_tolerating_manifest_errors(mounts)?;
    Ok(auth_mounts
        .iter()
        .map(|mount| row_for(catalog, store, mount))
        .collect())
}

fn row_for(
    catalog: &ProviderCatalog,
    store: &dyn CredentialStore,
    mount: &MountAuth,
) -> AuthStatusRow {
    let available = catalog
        .provider_auth_manifest_for(mount.config())
        .ok()
        .flatten()
        .map(|auth| scheme_options(&auth))
        .unwrap_or_default();
    AuthStatusRow {
        mount: mount.config().spec.mount.clone(),
        readiness: mount.readiness(store),
        available,
    }
}

struct SchemeOption {
    key: String,
    label: String,
    is_default: bool,
}

fn scheme_options(auth: &ProviderAuthManifest) -> Vec<SchemeOption> {
    auth.schemes
        .iter()
        .map(|(key, scheme)| SchemeOption {
            key: key.clone(),
            label: AuthMode::from_scheme(scheme)
                .map_or("unknown", AuthMode::label)
                .to_owned(),
            is_default: *key == auth.default,
        })
        .collect()
}

pub(super) struct AuthStatusRow {
    mount: String,
    readiness: AuthReadiness,
    available: Vec<SchemeOption>,
}

impl AuthStatusRow {
    fn available_line(&self) -> Option<String> {
        if self.available.is_empty() {
            return None;
        }
        let list = self
            .available
            .iter()
            .map(|opt| {
                if opt.is_default {
                    format!("{} ({}, default)", opt.key, opt.label)
                } else {
                    format!("{} ({})", opt.key, opt.label)
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!("schemes: {list}"))
    }

    fn text_detail(&self) -> String {
        match &self.readiness {
            AuthReadiness::None => "no auth required".to_owned(),
            AuthReadiness::ConfiguredExternally { source } => {
                format!("external credential ({source})")
            },
            AuthReadiness::Missing { .. } => {
                format!(
                    "missing credential; run `omnifs auth login {}` (`omnifs auth explain {}` for options)",
                    self.mount, self.mount
                )
            },
            AuthReadiness::Error { message } => format!("error: {message}"),
            AuthReadiness::Ready {
                kind,
                scopes,
                expires_at,
                refreshability,
                notices,
            } => {
                let mut detail = format!("{kind} ready");
                if !scopes.is_empty() {
                    let _ = write!(detail, "; scopes: {}", format_scopes(scopes));
                }
                if let Some(expires_at) = expires_at {
                    let _ = write!(detail, "; expires: {expires_at}");
                }
                if *refreshability != Refreshability::NotApplicable {
                    let _ = write!(detail, "; refresh: {refreshability}");
                }
                for notice in notices {
                    let _ = write!(detail, "; notice: {notice}");
                }
                detail
            },
        }
    }

    fn into_json(self) -> AuthEntryJson {
        let available_schemes = self.available.iter().map(|opt| opt.key.clone()).collect();
        AuthEntryJson {
            key: self.mount,
            auth: self.readiness,
            available_schemes,
        }
    }
}
