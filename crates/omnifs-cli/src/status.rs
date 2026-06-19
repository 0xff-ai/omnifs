//! Status report: data types, collection, and rendering.

use comfy_table::{Cell, ContentArrangement, Table, presets};
use omnifs_creds::{FileStore, Refreshability};
use std::fmt::Write as _;

use crate::{catalog::ProviderCatalog, paths::Paths};
use omnifs_api::DaemonStatus;

pub(crate) use crate::auth::AuthReadiness;
use crate::auth::AuthTerminalKind;
pub(crate) use crate::mount_report::{ProviderConfigStatus, ProviderReadyStatus, UserMountStatus};

#[derive(Debug, Clone)]
pub(crate) struct StatusReport {
    pub(crate) paths: Paths,
    /// Daemon runtime facts from the control API; `None` when no daemon
    /// answered on the control port.
    pub(crate) runtime: Option<DaemonStatus>,
    pub(crate) user_mounts: Vec<UserMountStatus>,
    pub(crate) providers: Vec<ProviderConfigStatus>,
}

pub(crate) fn collect_status(
    catalog: &ProviderCatalog,
    paths: Paths,
    runtime: Option<DaemonStatus>,
    mounts: Vec<crate::session::MountConfig>,
) -> StatusReport {
    let store = FileStore::new(&paths.credentials_file);
    StatusReport {
        runtime,
        user_mounts: catalog.scan_user_mount_configs(mounts.clone(), &store),
        providers: catalog.scan_provider_configs(mounts),
        paths,
    }
}

impl StatusReport {
    pub(crate) fn render(&self, detail: bool) -> String {
        let mut out = String::new();

        let version = format!("v{}", env!("CARGO_PKG_VERSION"));
        let header_width = 57usize;
        let left = "omnifs";
        let padding = header_width.saturating_sub(left.len() + version.len());
        let _ = writeln!(out, "{left}{version:>padding$}");
        let _ = writeln!(out, "{}", "─".repeat(header_width));

        let mut table = Table::new();
        table
            .load_preset(presets::NOTHING)
            .set_content_arrangement(ContentArrangement::Dynamic);
        table.add_row(vec![
            Cell::new("  runtime"),
            Cell::new("│"),
            Cell::new(match &self.runtime {
                Some(runtime) if runtime.mounts.is_empty() => "running (no mounts loaded)".into(),
                Some(runtime) => format!(
                    "running ({} loaded: {})",
                    runtime.mounts.len(),
                    runtime
                        .mounts
                        .iter()
                        .map(|mount| mount.mount.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                None => "not running".into(),
            }),
        ]);
        table.add_row(vec![
            Cell::new("  mount"),
            Cell::new("│"),
            Cell::new(format_mount(self)),
        ]);
        table.add_row(vec![
            Cell::new("  cache"),
            Cell::new("│"),
            Cell::new(Paths::display(&self.paths.cache_dir)),
        ]);
        let _ = writeln!(out, "{table}");
        let _ = writeln!(out);

        let _ = writeln!(out, "Mounts ({})", self.user_mounts.len());
        if self.user_mounts.is_empty() {
            let _ = writeln!(
                out,
                "  {}",
                crate::style::dim("(none — `omnifs init <provider>` to add one)")
            );
        } else {
            for mount in &self.user_mounts {
                write_mount_row(&mut out, mount);
            }
        }

        if detail {
            let _ = writeln!(out);
            write_runtime_providers(&mut out, &self.providers);
        } else if !self.providers.is_empty() {
            let _ = writeln!(out);
            let _ = writeln!(
                out,
                "{}",
                crate::style::dim("(use --detail for provider runtime detail)")
            );
        }

        out
    }
}

fn format_mount(report: &StatusReport) -> String {
    match &report.runtime {
        Some(runtime) => {
            let mp = Paths::display(&runtime.mount_point);
            match &runtime.frontend {
                Some(frontend) => format!("{mp} ({})", frontend.fs_type),
                None => format!("{mp} (not mounted)"),
            }
        },
        None => "—".to_string(),
    }
}

pub(crate) fn write_mount_row(out: &mut String, mount: &UserMountStatus) {
    match mount {
        UserMountStatus::Ready(m) => {
            let row = m.auth.terminal_row();
            let glyph = match row.kind {
                AuthTerminalKind::None => crate::style::dim("◯"),
                AuthTerminalKind::Ready => crate::style::success("●"),
                AuthTerminalKind::External => crate::style::warn("●"),
                AuthTerminalKind::Missing | AuthTerminalKind::Error => crate::style::error("●"),
            };
            let _ = writeln!(out, "  {glyph}  {:<14} {}", m.mount, row.summary);
        },
        UserMountStatus::Invalid { config_path, error } => {
            let name = config_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("<unknown>");
            let _ = writeln!(
                out,
                "  {}  {:<14} invalid ({error})",
                crate::style::error("●"),
                name
            );
        },
    }
}

pub(crate) fn write_runtime_providers(out: &mut String, providers: &[ProviderConfigStatus]) {
    let ready_count = providers
        .iter()
        .filter(|provider| {
            matches!(
                provider,
                ProviderConfigStatus::Ready(ProviderReadyStatus {
                    provider_present: true,
                    ..
                })
            )
        })
        .count();
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "providers: {} configured, {} ready",
        providers.len(),
        ready_count
    );

    if providers.is_empty() {
        let _ = writeln!(out, "- none");
        return;
    }

    for provider in providers {
        match provider {
            ProviderConfigStatus::Ready(provider) => {
                let _ = write!(
                    out,
                    "- {}: provider={} present={} metadata={} auth={} domains={} git_repos={}",
                    provider.mount,
                    provider.provider,
                    if provider.provider_present {
                        "yes"
                    } else {
                        "no"
                    },
                    if provider.metadata_available {
                        "yes"
                    } else {
                        "no"
                    },
                    provider.auth_count,
                    provider.domain_count,
                    provider.git_repo_count
                );
                if provider.root_mount {
                    let _ = write!(out, " root=yes");
                }
                if let Some(max_memory_mb) = provider.max_memory_mb {
                    let _ = write!(out, " max_memory={max_memory_mb}MiB");
                }
                let _ = writeln!(out);
            },
            ProviderConfigStatus::Invalid { config_path, error } => {
                let name = config_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("<unknown>");
                let _ = writeln!(out, "- {name}: invalid ({error})");
            },
        }
    }
}

// ── JSON output ──────────────────────────────────────────────────────────────

use serde::Serialize;

/// Serialize-only presentation DTO for `omnifs status --json`: the merged
/// host config + daemon runtime view. The daemon's wire types live in
/// `omnifs-api`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct StatusJson {
    pub version: String,
    pub runtime: RuntimeJson,
    pub mount: Option<MountJson>,
    pub paths: Paths,
    pub mounts: Vec<MountStatusJson>,
    pub providers: Vec<ProviderStatusJson>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum RuntimeJson {
    /// A daemon answered on the control API.
    Running {
        version: String,
        api_version: u32,
        pid: u32,
        executable: std::path::PathBuf,
        mount_point: std::path::PathBuf,
        config_dir: std::path::PathBuf,
        cache_dir: std::path::PathBuf,
        /// Mount names loaded in the daemon's registry.
        mounts: Vec<String>,
    },
    NotRunning,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct MountJson {
    pub source: String,
    pub mount_point: std::path::PathBuf,
    pub fs_type: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum MountStatusJson {
    Ready {
        config_path: std::path::PathBuf,
        mount: String,
        provider: String,
        provider_present: bool,
        metadata_available: bool,
        auth: AuthJson,
    },
    Invalid {
        config_path: std::path::PathBuf,
        error: String,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum AuthJson {
    None,
    Ready {
        kind: String,
        scopes: Vec<String>,
        expires_at: Option<String>,
        refreshability: Refreshability,
        notices: Vec<String>,
    },
    ConfiguredExternally {
        source: String,
    },
    Missing {
        command: String,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum ProviderStatusJson {
    Ready {
        config_path: std::path::PathBuf,
        mount: String,
        provider: String,
        provider_present: bool,
        metadata_available: bool,
        root_mount: bool,
        auth_count: usize,
        domain_count: usize,
        git_repo_count: usize,
        max_memory_mb: Option<u32>,
    },
    Invalid {
        config_path: std::path::PathBuf,
        error: String,
    },
}

impl StatusReport {
    pub(crate) fn to_json(&self) -> StatusJson {
        let runtime_json =
            self.runtime
                .as_ref()
                .map_or(RuntimeJson::NotRunning, |r| RuntimeJson::Running {
                    version: r.version.clone(),
                    api_version: r.api_version,
                    pid: r.pid,
                    executable: r.executable.clone(),
                    mount_point: r.mount_point.clone(),
                    config_dir: r.config_dir.clone(),
                    cache_dir: r.cache_dir.clone(),
                    mounts: r.mounts.iter().map(|mount| mount.mount.clone()).collect(),
                });
        StatusJson {
            version: env!("CARGO_PKG_VERSION").to_string(),
            runtime: runtime_json,
            mount: self.runtime.as_ref().and_then(|r| {
                r.frontend.as_ref().map(|frontend| MountJson {
                    source: frontend.source.clone(),
                    mount_point: r.mount_point.clone(),
                    fs_type: frontend.fs_type.clone(),
                })
            }),
            paths: self.paths.clone(),
            mounts: self.user_mounts.iter().map(mount_status_to_json).collect(),
            providers: self.providers.iter().map(provider_status_to_json).collect(),
        }
    }
}

fn mount_status_to_json(status: &UserMountStatus) -> MountStatusJson {
    match status {
        UserMountStatus::Ready(m) => MountStatusJson::Ready {
            config_path: m.config_path.clone(),
            mount: m.mount.clone(),
            provider: m.provider.clone(),
            provider_present: m.provider_present,
            metadata_available: m.metadata_available,
            auth: AuthJson::from(&m.auth),
        },
        UserMountStatus::Invalid { config_path, error } => MountStatusJson::Invalid {
            config_path: config_path.clone(),
            error: error.clone(),
        },
    }
}

impl From<&AuthReadiness> for AuthJson {
    fn from(auth: &AuthReadiness) -> Self {
        match auth {
            AuthReadiness::None => Self::None,
            AuthReadiness::Ready {
                kind,
                scopes,
                expires_at,
                refreshability,
                notices,
            } => Self::Ready {
                kind: kind.clone(),
                scopes: scopes.clone(),
                expires_at: expires_at.clone(),
                refreshability: *refreshability,
                notices: notices.clone(),
            },
            AuthReadiness::ConfiguredExternally { source } => Self::ConfiguredExternally {
                source: source.clone(),
            },
            AuthReadiness::Missing { command } => Self::Missing {
                command: command.clone(),
            },
            AuthReadiness::Error(error) => Self::Error {
                message: error.clone(),
            },
        }
    }
}

fn provider_status_to_json(status: &ProviderConfigStatus) -> ProviderStatusJson {
    match status {
        ProviderConfigStatus::Ready(s) => ProviderStatusJson::Ready {
            config_path: s.config_path.clone(),
            mount: s.mount.clone(),
            provider: s.provider.clone(),
            provider_present: s.provider_present,
            metadata_available: s.metadata_available,
            root_mount: s.root_mount,
            auth_count: s.auth_count,
            domain_count: s.domain_count,
            git_repo_count: s.git_repo_count,
            max_memory_mb: s.max_memory_mb,
        },
        ProviderConfigStatus::Invalid { config_path, error } => ProviderStatusJson::Invalid {
            config_path: config_path.clone(),
            error: error.clone(),
        },
    }
}
