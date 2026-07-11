//! Mount and provider scan results shared by status, doctor, and the catalog.

use omnifs_workspace::creds::CredentialStore;
use serde::Serialize;
use std::path::PathBuf;

use omnifs_workspace::mounts::Spec;
use omnifs_workspace::provider::Catalog;

use crate::auth::AuthReadiness;
use crate::mount_config::MountConfig;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ProviderReadyStatus {
    pub(crate) config_path: PathBuf,
    pub(crate) mount: String,
    pub(crate) provider: String,
    pub(crate) provider_present: bool,
    pub(crate) auth_count: usize,
    pub(crate) domain_count: usize,
    pub(crate) git_repo_count: usize,
    pub(crate) max_memory_mb: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum ProviderConfigStatus {
    Ready(ProviderReadyStatus),
    Invalid { config_path: PathBuf, error: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum UserMountStatus {
    Ready(UserMountReadyStatus),
    Invalid { config_path: PathBuf, error: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct UserMountReadyStatus {
    pub(crate) config_path: PathBuf,
    pub(crate) mount: String,
    pub(crate) provider: String,
    pub(crate) provider_present: bool,
    pub(crate) auth: AuthReadiness,
}

/// Whether the spec's pinned provider artifact is installed and its manifest
/// loads. `false` when the artifact is absent; an error when it is present but
/// its manifest is unreadable (a corrupt install), which the scans surface as
/// `Invalid`.
fn artifact_present(catalog: &Catalog, spec: &Spec) -> anyhow::Result<bool> {
    Ok(omnifs_workspace::mounts::pinned_manifest(catalog, spec)?.is_some())
}

pub(crate) fn scan_provider_configs(
    catalog: &Catalog,
    mounts: &[MountConfig],
) -> Vec<ProviderConfigStatus> {
    let mut providers = Vec::with_capacity(mounts.len());
    for configured in mounts {
        let config_path = configured.source.clone();
        let spec = &configured.config;
        let provider_present = match artifact_present(catalog, spec) {
            Ok(present) => present,
            Err(error) => {
                providers.push(ProviderConfigStatus::Invalid {
                    config_path,
                    error: error.to_string(),
                });
                continue;
            },
        };
        providers.push(ProviderConfigStatus::Ready(ProviderReadyStatus {
            config_path,
            mount: spec.mount.clone(),
            provider: spec.provider_name().to_string(),
            provider_present,
            auth_count: usize::from(spec.auth.is_some()),
            domain_count: spec
                .capabilities
                .as_ref()
                .and_then(|caps| caps.domains.as_ref())
                .map_or(0, |grant| grant.literal().len()),
            git_repo_count: spec
                .capabilities
                .as_ref()
                .and_then(|caps| caps.git_repos.as_ref())
                .map_or(0, |grant| grant.literal().len()),
            max_memory_mb: spec.limits.as_ref().and_then(|limits| limits.max_memory_mb),
        }));
    }
    providers
}

pub(crate) fn scan_user_mount_configs(
    catalog: &Catalog,
    mounts: &[MountConfig],
    store: &dyn CredentialStore,
) -> Vec<UserMountStatus> {
    let mut statuses = Vec::with_capacity(mounts.len());
    for configured in mounts {
        let config_path = configured.source.clone();
        match read_user_mount_status(catalog, configured, store) {
            Ok(status) => statuses.push(UserMountStatus::Ready(status)),
            Err(error) => statuses.push(UserMountStatus::Invalid {
                config_path,
                error: error.to_string(),
            }),
        }
    }
    statuses
}

fn read_user_mount_status(
    catalog: &Catalog,
    configured: &MountConfig,
    store: &dyn CredentialStore,
) -> anyhow::Result<UserMountReadyStatus> {
    let spec = &configured.config;
    let provider_present = artifact_present(catalog, spec)?;
    let auth = crate::auth::MountAuth::from_spec(catalog, spec.clone()).readiness(store);
    Ok(UserMountReadyStatus {
        config_path: configured.source.clone(),
        mount: spec.mount.clone(),
        provider: spec.provider_name().to_string(),
        provider_present,
        auth,
    })
}

/// The one mount-row renderer, shared by `omnifs status`'s Mounts section and
/// `omnifs mounts ls`, so both agree on shape and severity. Auth states split by
/// glyph so shape carries meaning without color: ready is a green liveness dot,
/// a mount needing no auth is a dim idle dot, a mount needing sign-in (missing
/// or expired) is a yellow `!` with the reauth command, and a credential-store
/// error or an invalid spec is a red `✗` with a fix. Invalid and healthy rows
/// share the same three-column grid, so an invalid row can no longer misalign.
pub(crate) fn mount_row(status: &UserMountStatus) -> crate::ui::report::Row {
    use crate::ui::report::Row;
    use crate::ui::style::Glyph;

    match status {
        UserMountStatus::Ready(mount) => {
            let reauth = format!("omnifs mounts reauth {}", mount.mount);
            let (glyph, value, fix): (Glyph, String, Option<String>) = match &mount.auth {
                AuthReadiness::None => (Glyph::IdleDot, "no auth needed".to_string(), None),
                AuthReadiness::Ready { .. } if ready_expired(&mount.auth) => (
                    Glyph::Warn,
                    format!("credential expired; run `{reauth}`"),
                    Some(reauth.clone()),
                ),
                // A healthy ready credential keeps the readiness summary
                // (kind, scopes, expiry) as its value.
                AuthReadiness::Ready { .. } => {
                    (Glyph::LiveDot, mount.auth.terminal_row().summary, None)
                },
                AuthReadiness::Missing { command } => (
                    Glyph::Warn,
                    format!("needs sign-in; run `{command}`"),
                    Some(command.clone()),
                ),
                AuthReadiness::Error { message } => (
                    Glyph::Fail,
                    format!("{message}; run `{reauth}`"),
                    Some(reauth.clone()),
                ),
            };
            let mut row = Row::new(glyph, mount.mount.clone(), value).identity();
            if let Some(fix) = fix {
                row = row.with_fix(fix);
            }
            row
        },
        UserMountStatus::Invalid { config_path, error } => {
            let name = config_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("<unknown>");
            Row::new(Glyph::Fail, name.to_string(), format!("invalid ({error})"))
                .identity()
                .with_fix("omnifs doctor")
        },
    }
}

/// Whether a `Ready` credential is expired and needs re-authentication now (as
/// opposed to a healthy credential that merely carries an informational
/// not-refreshable notice). Drives the `!` vs `●` split for ready mounts.
fn ready_expired(auth: &AuthReadiness) -> bool {
    matches!(
        auth,
        AuthReadiness::Ready { notices, .. }
            if notices.iter().any(|notice| notice.starts_with("expired"))
    )
}

/// Returns the first mount whose name starts with `target`.
pub(crate) fn closest_mount_name(mounts: &[MountConfig], target: &str) -> Option<String> {
    mounts
        .iter()
        .map(|mount| mount.name.to_string())
        .find(|name| name.starts_with(target))
}

#[cfg(test)]
mod golden {
    use super::*;
    use crate::ui::report::{Report, Section};
    use crate::ui::strip_ansi;
    use omnifs_workspace::creds::Refreshability;

    fn ready(mount: &str, scopes: &[&str], notices: &[&str]) -> UserMountStatus {
        UserMountStatus::Ready(UserMountReadyStatus {
            config_path: PathBuf::from(format!("/omnifs-home/mounts/{mount}.json")),
            mount: mount.to_string(),
            provider: mount.to_string(),
            provider_present: true,
            auth: AuthReadiness::Ready {
                kind: "oauth".to_string(),
                scopes: scopes.iter().map(ToString::to_string).collect(),
                expires_at: Some("2026-08-01T00:00:00Z".to_string()),
                refreshability: Refreshability::NotApplicable,
                notices: notices.iter().map(ToString::to_string).collect(),
            },
        })
    }

    /// The `mounts ls` grid: one shared row owner covering every auth severity,
    /// including an expired credential (a `!` warn split from healthy `●`) and
    /// an invalid spec whose columns line up with the healthy rows.
    #[test]
    fn mounts_ls_grid() {
        let statuses = vec![
            ready("github", &["repo", "read:org"], &[]),
            ready(
                "linear",
                &["read"],
                &["expired; run `omnifs mounts reauth linear`"],
            ),
            UserMountStatus::Ready(UserMountReadyStatus {
                config_path: PathBuf::from("/omnifs-home/mounts/rss.json"),
                mount: "rss".to_string(),
                provider: "rss".to_string(),
                provider_present: true,
                auth: AuthReadiness::None,
            }),
            UserMountStatus::Ready(UserMountReadyStatus {
                config_path: PathBuf::from("/omnifs-home/mounts/notion.json"),
                mount: "notion".to_string(),
                provider: "notion".to_string(),
                provider_present: true,
                auth: AuthReadiness::Missing {
                    command: "omnifs mounts reauth notion".to_string(),
                },
            }),
            UserMountStatus::Invalid {
                config_path: PathBuf::from("/omnifs-home/mounts/broken.json"),
                error: "provider artifact missing".to_string(),
            },
        ];
        let mut section = Section::new("Mounts").counted(statuses.len());
        for status in &statuses {
            section.push(mount_row(status));
        }
        let mut report = Report::new();
        report.push(section);
        insta::assert_snapshot!(strip_ansi(&report.render()));
    }

    #[test]
    fn mounts_ls_json_preserves_wire_schema() {
        let statuses = vec![
            ready("github", &["repo"], &[]),
            UserMountStatus::Ready(UserMountReadyStatus {
                config_path: PathBuf::from("/omnifs-home/mounts/rss.json"),
                mount: "rss".to_string(),
                provider: "rss".to_string(),
                provider_present: true,
                auth: AuthReadiness::None,
            }),
            UserMountStatus::Invalid {
                config_path: PathBuf::from("/omnifs-home/mounts/broken.json"),
                error: "provider artifact missing".to_string(),
            },
        ];
        insta::assert_snapshot!(serde_json::to_string_pretty(&statuses).unwrap());
    }
}
