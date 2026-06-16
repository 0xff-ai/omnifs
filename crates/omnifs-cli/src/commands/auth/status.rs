//! Auth status reporting.

use std::fmt::Write as _;

use omnifs_creds::{CredentialEntry, CredentialStore, Refreshability};

use super::shared::{format_rfc3339, format_scopes};
use crate::auth::{MountAuth, credential_notices};
use crate::catalog::ProviderCatalog;
use crate::paths::Paths;
use crate::session::MountConfig;

pub(super) fn status(
    paths: &Paths,
    catalog: &ProviderCatalog,
    mounts: Vec<MountConfig>,
    store: &dyn CredentialStore,
) -> anyhow::Result<()> {
    let rows = AuthStatus::new(catalog, store).load(mounts)?;
    anstream::println!("backend: {}", store.backend_label());
    if rows.is_empty() {
        anstream::println!("no mount configs found in {}", paths.config_file.display());
        return Ok(());
    }
    for row in rows {
        match row.text_detail() {
            Some(detail) => anstream::println!("{}: {detail}", row.mount),
            None => {
                anstream::println!(
                    "{}: missing credential; run `omnifs auth login {}`",
                    row.mount,
                    row.mount
                );
            },
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
    kind: String,
    scopes: Vec<String>,
    expires_at: Option<String>,
    refreshability: Refreshability,
    notices: Vec<String>,
}

pub(super) fn status_json(
    _paths: &Paths,
    catalog: &ProviderCatalog,
    mounts: Vec<MountConfig>,
    store: &dyn CredentialStore,
) -> anyhow::Result<()> {
    let entries = AuthStatus::new(catalog, store)
        .load(mounts)?
        .into_iter()
        .filter_map(AuthStatusRow::into_json)
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
        auth_mounts
            .iter()
            .map(|mount| self.row_for(mount))
            .collect()
    }

    fn row_for(&self, mount: &MountAuth) -> anyhow::Result<AuthStatusRow> {
        let entry = mount.status_entry(self.store)?;
        Ok(AuthStatusRow {
            mount: mount.config().spec.mount.clone(),
            entry,
        })
    }
}

pub(super) struct AuthStatusRow {
    mount: String,
    entry: Option<CredentialEntry>,
}

impl AuthStatusRow {
    fn reauth_command(&self) -> String {
        format!("omnifs auth login {}", self.mount)
    }

    fn text_detail(&self) -> Option<String> {
        let entry = self.entry.as_ref()?;
        let mut detail = format!("{} ready", entry.kind());
        if !entry.scopes().is_empty() {
            let _ = write!(detail, "; scopes: {}", format_scopes(entry.scopes()));
        }
        if let Some(expires_at) = entry.expires_at() {
            let _ = write!(detail, "; expires: {}", format_rfc3339(expires_at));
        }
        let refreshability = entry.refreshability();
        if refreshability != Refreshability::NotApplicable {
            let _ = write!(detail, "; refresh: {refreshability}");
        }
        for notice in credential_notices(entry, Some(&self.reauth_command())) {
            let _ = write!(detail, "; notice: {notice}");
        }
        Some(detail)
    }

    fn into_json(self) -> Option<AuthEntryJson> {
        let command = self.reauth_command();
        let entry = self.entry?;
        let expires_at = entry.expires_at().map(format_rfc3339);
        let refreshability = entry.refreshability();
        let notices = credential_notices(&entry, Some(&command));
        Some(AuthEntryJson {
            key: self.mount,
            kind: entry.kind().to_string(),
            scopes: entry.into_scopes(),
            expires_at,
            refreshability,
            notices,
        })
    }
}
