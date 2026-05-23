//! Status report: data types, collection, and rendering.

use comfy_table::{Cell, ContentArrangement, Table, presets};
use std::fmt::Write as _;
use std::path::PathBuf;

use crate::{catalog::ProviderCatalog, paths::Paths, proc_mounts};

pub(crate) use crate::auth_readiness::AuthReadiness;
use crate::auth_readiness::AuthTerminalKind;
pub(crate) use crate::mount_report::{ProviderConfigStatus, ProviderReadyStatus, UserMountStatus};

/// Canonical default for the FUSE mount point inside the container.
pub(crate) fn default_mount_point() -> PathBuf {
    PathBuf::from("/omnifs")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StatusReport {
    pub(crate) paths: Paths,
    pub(crate) mount_point: PathBuf,
    pub(crate) runtime: RuntimeStatus,
    pub(crate) mount: Option<proc_mounts::MountInfo>,
    pub(crate) user_mounts: Vec<UserMountStatus>,
    pub(crate) providers: Vec<ProviderConfigStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RuntimeStatus {
    Running(proc_mounts::RunningMountArgs),
    Mounted,
    NotRunning,
}

impl RuntimeStatus {
    fn from_mount(
        running_args: Option<proc_mounts::RunningMountArgs>,
        mount: Option<&proc_mounts::MountInfo>,
    ) -> Self {
        if let Some(args) = running_args {
            Self::Running(args)
        } else if mount.is_some() {
            Self::Mounted
        } else {
            Self::NotRunning
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Running(_) => "running",
            Self::Mounted => "mounted",
            Self::NotRunning => "not running",
        }
    }

    fn to_json(&self) -> RuntimeJson {
        match self {
            Self::Running(args) => match (&args.mount_point, &args.config_dir, &args.cache_dir) {
                (Some(mount_point), Some(config_dir), Some(cache_dir)) => RuntimeJson::Running {
                    mount_point: mount_point.clone(),
                    config_dir: config_dir.clone(),
                    cache_dir: cache_dir.clone(),
                },
                _ => RuntimeJson::Unknown,
            },
            Self::Mounted | Self::NotRunning => RuntimeJson::Unknown,
        }
    }
}

pub(crate) fn resolve_paths(
    mount_point: Option<String>,
    config_dir: Option<String>,
    cache_dir: Option<String>,
) -> (Paths, PathBuf) {
    use crate::paths::PathOverrides;
    use crate::runtime_state::RuntimeState;

    let config_dir_path = config_dir.as_ref().map(PathBuf::from);
    let persisted = config_dir_path
        .as_deref()
        .and_then(RuntimeState::load)
        .or_else(|| {
            let paths = Paths::resolve(PathOverrides {
                config_dir: config_dir_path.clone(),
                cache_dir: cache_dir.as_ref().map(PathBuf::from),
                ..Default::default()
            });
            RuntimeState::load(&paths.config_dir)
        });

    let inferred = persisted
        .map(|state| proc_mounts::RunningMountArgs {
            mount_point: Some(state.mount_point),
            config_dir: Some(state.config_dir),
            cache_dir: Some(state.cache_dir),
        })
        .or_else(proc_mounts::infer_running_mount_args)
        .unwrap_or_default();
    let config_dir_override = config_dir
        .clone()
        .map(PathBuf::from)
        .or(inferred.config_dir);
    let cache_dir_override = cache_dir.clone().map(PathBuf::from).or(inferred.cache_dir);
    let mount_point = mount_point
        .map(PathBuf::from)
        .or(inferred.mount_point)
        .unwrap_or_else(default_mount_point);
    let (paths, _config) = Paths::resolve_with_config(PathOverrides {
        config_dir: config_dir_override,
        cache_dir: cache_dir_override,
        ..Default::default()
    })
    .unwrap_or_else(|_| {
        // A malformed config.toml shouldn't crash `omnifs status`; fall back
        // to pure flag + env + platform-default resolution so the user can
        // still see what's broken.
        (
            Paths::resolve(PathOverrides {
                config_dir: config_dir.map(PathBuf::from),
                cache_dir: cache_dir.map(PathBuf::from),
                ..Default::default()
            }),
            crate::config::Config::default(),
        )
    });
    (paths, mount_point)
}

pub(crate) fn collect_status(
    catalog: &ProviderCatalog,
    paths: Paths,
    mount_point: PathBuf,
) -> anyhow::Result<StatusReport> {
    let store = crate::session::open_store(&paths.credentials_file, true);
    let running_args = crate::runtime_state::RuntimeState::load(&paths.config_dir)
        .map(|state| proc_mounts::RunningMountArgs {
            mount_point: Some(state.mount_point),
            config_dir: Some(state.config_dir),
            cache_dir: Some(state.cache_dir),
        })
        .or_else(proc_mounts::infer_running_mount_args);
    let mount = proc_mounts::find_mount(&mount_point)?;
    let runtime = RuntimeStatus::from_mount(running_args, mount.as_ref());
    Ok(StatusReport {
        runtime,
        mount,
        user_mounts: catalog.scan_user_mount_configs(store.as_ref())?,
        providers: catalog.scan_provider_configs()?,
        mount_point,
        paths,
    })
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
            Cell::new(self.runtime.label()),
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
    let mp = Paths::display(&report.mount_point);
    match &report.mount {
        Some(mount) => format!("{mp} ({})", mount.fs_type),
        None => format!("{mp} (not mounted)"),
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

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StatusJson {
    pub version: String,
    pub runtime: RuntimeJson,
    pub mount: Option<MountJson>,
    pub paths: PathsJson,
    pub mounts: Vec<MountStatusJson>,
    /// Always present (may be empty) so that runtime status verification can rely on it.
    pub providers: Vec<ProviderStatusJson>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum RuntimeJson {
    /// Daemon detected via `/proc/{pid}/cmdline` (only inside the container).
    Running {
        mount_point: std::path::PathBuf,
        config_dir: std::path::PathBuf,
        cache_dir: std::path::PathBuf,
    },
    /// Host-side status invocation: runtime container probing is `omnifs doctor`'s job.
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MountJson {
    pub source: String,
    pub mount_point: std::path::PathBuf,
    pub fs_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PathsJson {
    pub mount_point: std::path::PathBuf,
    pub config_dir: std::path::PathBuf,
    pub data_dir: std::path::PathBuf,
    pub cache_dir: std::path::PathBuf,
    pub mounts_dir: std::path::PathBuf,
    pub providers_dir: std::path::PathBuf,
    pub credentials_file: std::path::PathBuf,
    pub config_file: std::path::PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum AuthJson {
    None,
    Ready {
        kind: String,
        scopes: Vec<String>,
        expires_at: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
        StatusJson {
            version: env!("CARGO_PKG_VERSION").to_string(),
            runtime: self.runtime.to_json(),
            mount: self.mount.as_ref().map(|m| MountJson {
                source: m.source.clone(),
                mount_point: m.mount_point.clone(),
                fs_type: m.fs_type.clone(),
            }),
            paths: PathsJson {
                mount_point: self.mount_point.clone(),
                config_dir: self.paths.config_dir.clone(),
                data_dir: self.paths.data_dir.clone(),
                cache_dir: self.paths.cache_dir.clone(),
                mounts_dir: self.paths.mounts_dir.clone(),
                providers_dir: self.paths.providers_dir.clone(),
                credentials_file: self.paths.credentials_file.clone(),
                config_file: self.paths.config_file.clone(),
            },
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
            } => Self::Ready {
                kind: kind.clone(),
                scopes: scopes.clone(),
                expires_at: expires_at.clone(),
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
