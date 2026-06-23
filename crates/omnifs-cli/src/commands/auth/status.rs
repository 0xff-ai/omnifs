//! Auth status reporting.

use std::fmt::Write as _;

use omnifs_creds::{CredentialStore, Refreshability};

use super::shared::format_scopes;
use crate::auth::explain::AuthMode;
use crate::auth::{AuthReadiness, AuthReadinessJson, MountAuth};
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
    let rows = AuthStatus::new(catalog, store).load(mounts)?;
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
    auth: AuthReadinessJson,
    available_schemes: Vec<String>,
}

pub(super) fn status_json(
    catalog: &ProviderCatalog,
    mounts: Vec<MountConfig>,
    store: &dyn CredentialStore,
) -> anyhow::Result<()> {
    let entries = AuthStatus::new(catalog, store)
        .load(mounts)?
        .into_iter()
        .map(AuthStatusRow::into_json)
        .collect();
    let payload = AuthStatusJson { entries };
    anstream::println!("{}", serde_json::to_string(&payload)?);
    Ok(())
}

pub(super) struct AuthStatus<'a> {
    catalog: &'a ProviderCatalog,
    store: &'a dyn CredentialStore,
}

impl<'a> AuthStatus<'a> {
    fn new(catalog: &'a ProviderCatalog, store: &'a dyn CredentialStore) -> Self {
        Self { catalog, store }
    }

    fn load(&self, mounts: Vec<MountConfig>) -> anyhow::Result<Vec<AuthStatusRow>> {
        let auth_mounts = self
            .catalog
            .load_all_mount_auth_tolerating_manifest_errors(mounts)?;
        Ok(auth_mounts
            .iter()
            .map(|mount| self.row_for(mount))
            .collect())
    }

    fn row_for(&self, mount: &MountAuth) -> AuthStatusRow {
        let available = self
            .catalog
            .provider_auth_manifest_for(mount.config())
            .ok()
            .flatten()
            .map(|auth| scheme_options(&auth))
            .unwrap_or_default();
        AuthStatusRow {
            mount: mount.config().spec.mount.clone(),
            readiness: mount.readiness(self.store),
            available,
        }
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
            AuthReadiness::Error(error) => format!("error: {error}"),
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
            auth: AuthReadinessJson::from(&self.readiness),
            available_schemes,
        }
    }
}
