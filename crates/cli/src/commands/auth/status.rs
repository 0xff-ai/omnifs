//! Auth status reporting.

use std::fmt::Write as _;

use omnifs_creds::{CredentialEntry, CredentialStore};
use omnifs_host::config::AuthConfig;

use super::shared::{
    MountConfig, format_rfc3339, format_scopes, load_all_mounts, primary_auth, read_auth_manifest,
};
use crate::auth_manifest_view::AuthManifestView;
use crate::catalog::ProviderCatalog;
use crate::credential_target::CredentialTarget;
use crate::paths::Paths;

pub(super) fn status(
    paths: &Paths,
    catalog: &ProviderCatalog,
    store: &dyn CredentialStore,
) -> anyhow::Result<()> {
    let rows = AuthStatus::new(catalog, store).load()?;
    anstream::println!("backend: {}", store.backend_label());
    if rows.is_empty() {
        anstream::println!("no mount configs found in {}", paths.mounts_dir.display());
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
}

pub(super) fn status_json(
    _paths: &Paths,
    catalog: &ProviderCatalog,
    store: &dyn CredentialStore,
) -> anyhow::Result<()> {
    let entries = AuthStatus::new(catalog, store)
        .load()?
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

    fn load(&self) -> anyhow::Result<Vec<AuthStatusRow>> {
        load_all_mounts(self.catalog)?
            .into_iter()
            .map(|mount| self.row_for(mount))
            .collect()
    }

    fn row_for(&self, mount: MountConfig) -> anyhow::Result<AuthStatusRow> {
        let auth = primary_auth(&mount.config);
        let manifest = read_auth_manifest(self.catalog, &mount.config)
            .ok()
            .flatten();
        let view = AuthManifestView::new(manifest.as_ref());
        let scheme = view
            .oauth_scheme(auth.and_then(AuthConfig::scheme))
            .ok()
            .map(|scheme| scheme.key.clone())
            .or_else(|| {
                view.static_token_scheme_key(None, auth.and_then(AuthConfig::scheme))
                    .ok()
            })
            .unwrap_or_else(|| "unknown".to_owned());
        let target = CredentialTarget::for_scheme(&mount.config, auth, &scheme, None)?;
        let entry = target.lookup(self.store)?;
        Ok(AuthStatusRow {
            mount: mount.config.mount,
            entry,
        })
    }
}

pub(super) struct AuthStatusRow {
    mount: String,
    entry: Option<CredentialEntry>,
}

impl AuthStatusRow {
    fn text_detail(&self) -> Option<String> {
        let entry = self.entry.as_ref()?;
        let mut detail = format!("{} ready", entry.kind());
        if !entry.scopes().is_empty() {
            let _ = write!(detail, "; scopes: {}", format_scopes(entry.scopes()));
        }
        if let Some(expires_at) = entry.expires_at() {
            let _ = write!(detail, "; expires: {}", format_rfc3339(expires_at));
        }
        Some(detail)
    }

    fn into_json(self) -> Option<AuthEntryJson> {
        let entry = self.entry?;
        let expires_at = entry.expires_at().map(format_rfc3339);
        Some(AuthEntryJson {
            key: self.mount,
            kind: entry.kind().to_string(),
            scopes: entry.into_scopes(),
            expires_at,
        })
    }
}
