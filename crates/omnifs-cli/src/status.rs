//! Status report: data types, collection, and rendering.

use omnifs_workspace::creds::FileStore;
use std::fmt::Write as _;

use omnifs_api::{
    CredentialHealth, DaemonHealth, DaemonStatus, DaemonSubsystem, FsType, HealthState,
    SubsystemHealth,
};
use omnifs_workspace::layout::WorkspaceLayout;
use omnifs_workspace::provider::Catalog;

use crate::auth::AuthTerminalKind;
use crate::error::ExitCode;
pub(crate) use crate::mount_report::{ProviderConfigStatus, ProviderReadyStatus, UserMountStatus};

#[derive(Debug, Clone)]
pub(crate) struct StatusReport {
    pub(crate) paths: WorkspaceLayout,
    /// Daemon runtime facts from the control API; `None` when no daemon
    /// answered on the control port.
    pub(crate) runtime: Option<DaemonStatus>,
    pub(crate) user_mounts: Vec<UserMountStatus>,
    pub(crate) providers: Vec<ProviderConfigStatus>,
}

impl StatusReport {
    pub(crate) fn collect(
        catalog: &Catalog,
        paths: WorkspaceLayout,
        runtime: Option<DaemonStatus>,
        mounts: Vec<crate::mount_config::MountConfig>,
    ) -> Self {
        let store = FileStore::new(&paths.credentials_file);
        Self {
            runtime,
            user_mounts: crate::mount_report::scan_user_mount_configs(
                catalog,
                mounts.clone(),
                &store,
            ),
            providers: crate::mount_report::scan_provider_configs(catalog, mounts),
            paths,
        }
    }

    pub(crate) fn render(&self, detail: bool) -> String {
        let mut out = String::new();

        let version = format!("v{}", env!("CARGO_PKG_VERSION"));
        let header_width = 57usize;
        let left = "omnifs";
        let padding = header_width.saturating_sub(left.len() + version.len());
        let _ = writeln!(out, "{left}{version:>padding$}");
        let _ = writeln!(out, "{}", "─".repeat(header_width));

        let _ = writeln!(
            out,
            "  {:<7} │ {}",
            "runtime",
            format_runtime(self.runtime.as_ref())
        );
        let _ = writeln!(out, "  {:<7} │ {}", "mount", self.format_mount());
        let _ = writeln!(
            out,
            "  {:<7} │ {}",
            "cache",
            WorkspaceLayout::display(&self.paths.cache_dir)
        );

        if let Some(runtime) = &self.runtime
            && !runtime.health.subsystems.is_empty()
        {
            write_daemon_health(&mut out, &runtime.health);
        }

        // Surface mounts that did not converge at the last reconcile. A dark
        // mount is visible here with its failure reason, not silently absent
        // from the mounts list below.
        if let Some(runtime) = &self.runtime
            && !runtime.failed.is_empty()
        {
            let _ = writeln!(out);
            let _ = writeln!(out, "Failed mounts ({}):", runtime.failed.len());
            for failure in &runtime.failed {
                let _ = writeln!(
                    out,
                    "  {}  {:<14} {}",
                    crate::style::error("●"),
                    failure.mount,
                    failure.reason
                );
            }
        }

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
                Self::write_mount_row(&mut out, mount);
            }
        }

        if detail {
            let _ = writeln!(out);
            write_configured_providers(&mut out, &self.providers);
        } else if !self.providers.is_empty() {
            let _ = writeln!(out);
            let _ = writeln!(
                out,
                "{}",
                crate::style::dim("(use --detail for configured provider detail)")
            );
        }

        out
    }

    pub(crate) fn exit_code(&self) -> ExitCode {
        if self.is_degraded() {
            ExitCode::Degraded
        } else {
            ExitCode::Success
        }
    }

    fn is_degraded(&self) -> bool {
        if let Some(runtime) = &self.runtime
            && (!runtime.failed.is_empty()
                || matches!(
                    runtime.health.overall_state(),
                    HealthState::Degraded | HealthState::Unhealthy
                )
                || runtime.mounts.iter().any(|mount| {
                    mount
                        .auth_health
                        .is_some_and(|health| health != CredentialHealth::Ready)
                }))
        {
            return true;
        }

        self.user_mounts.iter().any(|mount| match mount {
            UserMountStatus::Ready(mount) => matches!(
                mount.auth.terminal_row().kind,
                AuthTerminalKind::Missing | AuthTerminalKind::Error
            ),
            UserMountStatus::Invalid { .. } => true,
        })
    }

    fn format_mount(&self) -> String {
        match &self.runtime {
            Some(runtime) if !runtime.health.subsystems.is_empty() => runtime
                .health
                .subsystem(DaemonSubsystem::Frontend)
                .map_or_else(
                    || WorkspaceLayout::display(&runtime.mount_point),
                    |frontend| frontend.message.clone(),
                ),
            Some(runtime) => {
                let mp = WorkspaceLayout::display(&runtime.mount_point);
                match &runtime.frontend {
                    Some(frontend) => format!("{mp} ({})", frontend.fs_type),
                    None => format!("{mp} (not mounted)"),
                }
            },
            None => "—".to_string(),
        }
    }

    fn write_mount_row(out: &mut String, mount: &UserMountStatus) {
        match mount {
            UserMountStatus::Ready(m) => {
                let row = m.auth.terminal_row();
                let glyph = match row.kind {
                    AuthTerminalKind::None => crate::style::dim("◯"),
                    AuthTerminalKind::Ready => crate::style::success("●"),
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
}

fn format_runtime(runtime: Option<&DaemonStatus>) -> String {
    let Some(runtime) = runtime else {
        return "not running".into();
    };
    if runtime.health.subsystems.is_empty() {
        return "running".into();
    }
    format!(
        "running ({})",
        health_state_label(runtime.health.overall_state())
    )
}

fn write_daemon_health(out: &mut String, health: &DaemonHealth) {
    let _ = writeln!(out);
    let _ = writeln!(out, "Daemon health");
    for subsystem in &health.subsystems {
        write_health_row(out, subsystem);
    }
}

fn write_health_row(out: &mut String, subsystem: &SubsystemHealth) {
    let glyph = match subsystem.state {
        HealthState::Healthy => crate::style::success("●"),
        HealthState::Starting | HealthState::Degraded => crate::style::warn("●"),
        HealthState::Unhealthy => crate::style::error("●"),
    };
    let _ = writeln!(
        out,
        "  {glyph}  {:<10} {}",
        daemon_subsystem_label(subsystem.subsystem),
        subsystem.message
    );
}

fn daemon_subsystem_label(subsystem: DaemonSubsystem) -> &'static str {
    match subsystem {
        DaemonSubsystem::Control => "control",
        DaemonSubsystem::Backend => "backend",
        DaemonSubsystem::Frontend => "frontend",
        DaemonSubsystem::Mounts => "mounts",
    }
}

fn health_state_label(state: HealthState) -> &'static str {
    match state {
        HealthState::Starting => "starting",
        HealthState::Healthy => "healthy",
        HealthState::Degraded => "degraded",
        HealthState::Unhealthy => "unhealthy",
    }
}

pub(crate) fn write_configured_providers(out: &mut String, providers: &[ProviderConfigStatus]) {
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
        "configured providers: {} configured, {} ready",
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
                    "- {}: provider={} present={} auth={} domains={} git_repos={}",
                    provider.mount,
                    provider.provider,
                    if provider.provider_present {
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
    pub paths: WorkspaceLayout,
    pub mounts: Vec<UserMountStatus>,
    pub providers: Vec<ProviderConfigStatus>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum RuntimeJson {
    /// A daemon answered on the control API.
    Running {
        version: String,
        api_major: u16,
        api_minor: u16,
        pid: u32,
        executable: std::path::PathBuf,
        mount_point: std::path::PathBuf,
        config_dir: std::path::PathBuf,
        cache_dir: std::path::PathBuf,
        health: DaemonHealth,
        /// Mount names loaded in the daemon's registry.
        mounts: Vec<String>,
        /// Mounts that did not converge at the last reconcile. Empty when every
        /// desired mount is serving; a dark mount appears here with its reason.
        failed_mounts: Vec<FailedMountJson>,
    },
    NotRunning,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct FailedMountJson {
    pub mount: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct MountJson {
    pub source: String,
    pub mount_point: std::path::PathBuf,
    pub fs_type: FsType,
}

impl StatusReport {
    pub(crate) fn to_json(&self) -> StatusJson {
        let runtime_json =
            self.runtime
                .as_ref()
                .map_or(RuntimeJson::NotRunning, |r| RuntimeJson::Running {
                    version: r.version.clone(),
                    api_major: r.api_major,
                    api_minor: r.api_minor,
                    pid: r.pid,
                    executable: r.executable.clone(),
                    mount_point: r.mount_point.clone(),
                    config_dir: r.config_dir.clone(),
                    cache_dir: r.cache_dir.clone(),
                    health: r.health.clone(),
                    mounts: r.mounts.iter().map(|mount| mount.mount.clone()).collect(),
                    failed_mounts: r
                        .failed
                        .iter()
                        .map(|f| FailedMountJson {
                            mount: f.mount.clone(),
                            reason: f.reason.clone(),
                        })
                        .collect(),
                });
        StatusJson {
            version: env!("CARGO_PKG_VERSION").to_string(),
            runtime: runtime_json,
            mount: self.runtime.as_ref().and_then(|r| {
                r.frontend.as_ref().map(|frontend| MountJson {
                    source: frontend.source.clone(),
                    mount_point: r.mount_point.clone(),
                    fs_type: frontend.fs_type,
                })
            }),
            paths: self.paths.clone(),
            mounts: self.user_mounts.clone(),
            providers: self.providers.clone(),
        }
    }
}
