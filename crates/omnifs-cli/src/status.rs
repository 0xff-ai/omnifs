//! Status report: data types, collection, and rendering.

use std::fmt::Write as _;

use omnifs_workspace::creds::FileStore;

use omnifs_api::{
    CredentialHealth, DaemonHealth, DaemonStatus, DaemonSubsystem, FrontendDelivery, FrontendInfo,
    FsType, HealthState, SubsystemHealth,
};
use omnifs_workspace::layout::WorkspaceLayout;
use omnifs_workspace::provider::Catalog;

use crate::auth::AuthTerminalKind;
use crate::error::ExitCode;
pub(crate) use crate::mount_report::{ProviderConfigStatus, UserMountStatus};
use crate::ui::report::{Report, Row, Section};
use crate::ui::style::Glyph;

#[derive(Debug, Clone)]
pub(crate) struct StatusReport {
    pub(crate) paths: WorkspaceLayout,
    /// Daemon runtime facts from the control API; `None` when no daemon
    /// answered on the control port. `runtime.frontends` is the live
    /// attachment registry: every frontend `omnifs status` reports comes
    /// from here, not from the on-disk runtime record (`omnifs frontend
    /// status` reports additional un-attached backend health).
    pub(crate) runtime: Option<DaemonStatus>,
    pub(crate) user_mounts: Vec<UserMountStatus>,
    pub(crate) providers: Vec<ProviderConfigStatus>,
}

impl StatusReport {
    pub(crate) fn collect(
        catalog: &Catalog,
        paths: WorkspaceLayout,
        runtime: Option<DaemonStatus>,
        mounts: &[crate::mount_config::MountConfig],
    ) -> Self {
        let store = FileStore::new(&paths.credentials_file);
        Self {
            runtime,
            user_mounts: crate::mount_report::scan_user_mount_configs(catalog, mounts, &store),
            providers: crate::mount_report::scan_provider_configs(catalog, mounts),
            paths,
        }
    }

    /// The flat human report: a version and daemon block, a
    /// `Frontends` section when any are attached, a `Mounts` section whose auth
    /// states split by glyph, and dark-state sections (daemon health, failed
    /// mounts) only when there is something to show. `--detail` appends the
    /// configured-provider section.
    pub(crate) fn build_report(&self, detail: bool) -> Report {
        let mut report = Report::new();
        report.push(self.head_section());

        {
            let frontends = self
                .runtime
                .as_ref()
                .map_or(&[][..], |runtime| runtime.frontends.as_slice());
            let mut section = Section::new("Frontends").counted(frontends.len());
            if frontends.is_empty() {
                section.push(Row::new(Glyph::Skip, "", "no frontends attached"));
            }
            for frontend in frontends {
                section.push(frontend_row(frontend));
            }
            report.push(section);
        }

        // A healthy daemon omits the subsystem block (matching 4.2); a subsystem
        // that is not healthy surfaces here with its severity glyph.
        if let Some(runtime) = &self.runtime {
            let unhealthy: Vec<&SubsystemHealth> = runtime
                .health
                .subsystems
                .iter()
                .filter(|subsystem| subsystem.state != HealthState::Healthy)
                .collect();
            if !unhealthy.is_empty() {
                let mut section = Section::new("Daemon health");
                for subsystem in unhealthy {
                    section.push(health_row(subsystem));
                }
                report.push(section);
            }
        }

        // Mounts that did not converge at the last reconcile: a dark mount is
        // visible with its reason, not silently absent from the list below.
        if let Some(runtime) = &self.runtime
            && !runtime.failed.is_empty()
        {
            let mut section = Section::new("Failed mounts").counted(runtime.failed.len());
            for failure in &runtime.failed {
                section.push(
                    Row::new(Glyph::Fail, failure.mount.clone(), failure.reason.clone())
                        .identity()
                        .with_fix("omnifs logs"),
                );
            }
            report.push(section);
        }

        let mut mounts = Section::new("Mounts").counted(self.user_mounts.len());
        if self.user_mounts.is_empty() {
            mounts.push(Row::new(
                Glyph::Skip,
                "",
                "no mounts configured; run `omnifs init <provider>`",
            ));
        } else {
            for mount in &self.user_mounts {
                mounts.push(crate::mount_report::mount_row(mount));
            }
        }
        report.push(mounts);

        if detail {
            report.push(self.provider_section());
        }

        report
    }

    /// The version line plus the daemon and home rows. Uptime is intentionally
    /// absent: the control API does not expose an authoritative start time, and
    /// the local runtime record may describe a different or remote endpoint.
    fn head_section(&self) -> Section {
        let mut head = Section::new(format!("omnifs {}", env!("CARGO_PKG_VERSION")));
        match &self.runtime {
            Some(runtime) => {
                let (glyph, suffix) = match runtime.health.overall_state() {
                    HealthState::Healthy | HealthState::Starting => (Glyph::Done, String::new()),
                    state @ (HealthState::Degraded | HealthState::Unhealthy) => {
                        (Glyph::Warn, format!(", {}", health_state_label(state)))
                    },
                };
                head.push(Row::new(
                    glyph,
                    "daemon",
                    format!(
                        "running (pid {}, api {}.{}{suffix})",
                        runtime.pid, runtime.api_major, runtime.api_minor
                    ),
                ));
            },
            None => head.push(
                Row::new(Glyph::Skip, "daemon", "not running; run `omnifs up`")
                    .with_fix("omnifs up"),
            ),
        }
        head.push(Row::new(
            Glyph::Skip,
            "home",
            WorkspaceLayout::display(&self.paths.config_dir),
        ));
        head
    }

    /// The `--detail` configured-provider section, one row per configured mount.
    fn provider_section(&self) -> Section {
        let mut section = Section::new("Configured providers").counted(self.providers.len());
        for provider in &self.providers {
            match provider {
                ProviderConfigStatus::Ready(provider) => {
                    let glyph = if provider.provider_present {
                        Glyph::Done
                    } else {
                        Glyph::Warn
                    };
                    let mut value = format!(
                        "{} present={} auth={} domains={} git_repos={}",
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
                    if let Some(max_memory_mb) = provider.max_memory_mb {
                        let _ = write!(value, " max_memory={max_memory_mb}MiB");
                    }
                    section.push(Row::new(glyph, provider.mount.clone(), value).identity());
                },
                ProviderConfigStatus::Invalid { config_path, error } => {
                    let name = config_path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("<unknown>");
                    section.push(
                        Row::new(Glyph::Fail, name.to_string(), format!("invalid ({error})"))
                            .identity(),
                    );
                },
            }
        }
        section
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
                        .is_some_and(CredentialHealth::needs_attention)
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
}

/// One row per live frontend attachment. Docker and krunkit mount points live
/// inside their guest, so they are marked `(guest)` rather than presented as
/// host-visible paths. The green liveness dot means attached.
fn frontend_row(frontend: &FrontendInfo) -> Row {
    let location = WorkspaceLayout::display(&frontend.mount_point);
    let location = match frontend.delivery {
        FrontendDelivery::Local => location,
        FrontendDelivery::Docker | FrontendDelivery::Krunkit => format!("{location} (guest)"),
    };
    Row::new(
        Glyph::LiveDot,
        format!("{} ({})", frontend.fs_type, frontend.delivery),
        format!("{location}  attached"),
    )
    .identity()
}

/// One subsystem-health row, shown only when the subsystem is not healthy. A
/// starting or degraded subsystem warns; an unhealthy one fails.
fn health_row(subsystem: &SubsystemHealth) -> Row {
    let glyph = match subsystem.state {
        HealthState::Healthy => Glyph::LiveDot,
        HealthState::Starting | HealthState::Degraded => Glyph::Warn,
        HealthState::Unhealthy => Glyph::Fail,
    };
    Row::new(
        glyph,
        daemon_subsystem_label(subsystem.subsystem),
        subsystem.message.clone(),
    )
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
    /// Every live frontend attachment, local and guest alike.
    pub frontends: Vec<FrontendJson>,
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

#[derive(Debug, Clone, Serialize)]
pub(crate) struct FrontendJson {
    pub fs_type: FsType,
    pub delivery: FrontendDelivery,
    pub mount_point: std::path::PathBuf,
}

impl From<&FrontendInfo> for FrontendJson {
    fn from(frontend: &FrontendInfo) -> Self {
        Self {
            fs_type: frontend.fs_type,
            delivery: frontend.delivery,
            mount_point: frontend.mount_point.clone(),
        }
    }
}

impl StatusReport {
    pub(crate) fn into_json(self) -> StatusJson {
        // `mount` names the first LOCAL attachment specifically (source and
        // fs_type must come from that same entry); the path itself is the
        // daemon-derived `mount_point` verbatim, never re-derived
        // client-side from the frontends list.
        let mount = self.runtime.as_ref().and_then(|runtime| {
            if runtime.mount_point.as_os_str().is_empty() {
                return None;
            }
            let local = runtime
                .frontends
                .iter()
                .find(|frontend| frontend.delivery == FrontendDelivery::Local)?;
            Some(MountJson {
                source: local.source.clone(),
                mount_point: runtime.mount_point.clone(),
                fs_type: local.fs_type,
            })
        });
        let frontends = self.runtime.as_ref().map_or_else(Vec::new, |runtime| {
            runtime.frontends.iter().map(FrontendJson::from).collect()
        });
        let runtime_json = self
            .runtime
            .map_or(RuntimeJson::NotRunning, |r| RuntimeJson::Running {
                version: r.version,
                api_major: r.api_major,
                api_minor: r.api_minor,
                pid: r.pid,
                executable: r.executable,
                mount_point: r.mount_point,
                config_dir: r.config_dir,
                cache_dir: r.cache_dir,
                health: r.health,
                mounts: r.mounts.into_iter().map(|mount| mount.mount).collect(),
                failed_mounts: r
                    .failed
                    .into_iter()
                    .map(|f| FailedMountJson {
                        mount: f.mount,
                        reason: f.reason,
                    })
                    .collect(),
            });
        StatusJson {
            version: env!("CARGO_PKG_VERSION").to_string(),
            runtime: runtime_json,
            mount,
            frontends,
            paths: self.paths,
            mounts: self.user_mounts,
            providers: self.providers,
        }
    }
}

#[cfg(test)]
mod golden {
    use super::*;
    use crate::auth::AuthReadiness;
    use crate::mount_report::UserMountReadyStatus;
    use crate::ui::strip_ansi;
    use omnifs_api::DaemonBackend;
    use omnifs_workspace::creds::Refreshability;
    use std::path::PathBuf;

    fn layout() -> WorkspaceLayout {
        WorkspaceLayout {
            config_dir: PathBuf::from("/omnifs-home"),
            cache_dir: PathBuf::from("/omnifs-home/cache"),
            mounts_dir: PathBuf::from("/omnifs-home/mounts"),
            providers_dir: PathBuf::from("/omnifs-home/providers"),
            credentials_file: PathBuf::from("/omnifs-home/credentials.json"),
            config_file: PathBuf::from("/omnifs-home/config.toml"),
        }
    }

    fn ready(mount: &str, scopes: &[&str], expires: Option<&str>) -> UserMountStatus {
        UserMountStatus::Ready(UserMountReadyStatus {
            config_path: PathBuf::from(format!("/omnifs-home/mounts/{mount}.json")),
            mount: mount.to_string(),
            provider: mount.to_string(),
            provider_present: true,
            auth: AuthReadiness::Ready {
                kind: "oauth".to_string(),
                scopes: scopes.iter().map(ToString::to_string).collect(),
                expires_at: expires.map(ToString::to_string),
                refreshability: Refreshability::NotApplicable,
                notices: Vec::new(),
            },
        })
    }

    fn no_auth(mount: &str) -> UserMountStatus {
        UserMountStatus::Ready(UserMountReadyStatus {
            config_path: PathBuf::from(format!("/omnifs-home/mounts/{mount}.json")),
            mount: mount.to_string(),
            provider: mount.to_string(),
            provider_present: true,
            auth: AuthReadiness::None,
        })
    }

    fn needs_signin(mount: &str) -> UserMountStatus {
        UserMountStatus::Ready(UserMountReadyStatus {
            config_path: PathBuf::from(format!("/omnifs-home/mounts/{mount}.json")),
            mount: mount.to_string(),
            provider: mount.to_string(),
            provider_present: true,
            auth: AuthReadiness::Missing {
                command: format!("omnifs mounts reauth {mount}"),
            },
        })
    }

    fn store_error(mount: &str) -> UserMountStatus {
        UserMountStatus::Ready(UserMountReadyStatus {
            config_path: PathBuf::from(format!("/omnifs-home/mounts/{mount}.json")),
            mount: mount.to_string(),
            provider: mount.to_string(),
            provider_present: true,
            auth: AuthReadiness::Error {
                message: "credential store unreadable".to_string(),
            },
        })
    }

    fn invalid() -> UserMountStatus {
        UserMountStatus::Invalid {
            config_path: PathBuf::from("/omnifs-home/mounts/broken.json"),
            error: "provider artifact missing".to_string(),
        }
    }

    fn mounts() -> Vec<UserMountStatus> {
        vec![
            ready(
                "github",
                &["repo", "read:org"],
                Some("2026-08-01T00:00:00Z"),
            ),
            no_auth("rss"),
            needs_signin("linear"),
            store_error("notion"),
            invalid(),
        ]
    }

    fn frontend(fs_type: FsType, delivery: FrontendDelivery, mount_point: &str) -> FrontendInfo {
        FrontendInfo {
            source: "provider".to_string(),
            fs_type,
            mount_point: PathBuf::from(mount_point),
            delivery,
        }
    }

    fn running_runtime() -> DaemonStatus {
        DaemonStatus {
            version: "0.2.1".to_string(),
            api_major: 2,
            api_minor: 1,
            pid: 41231,
            instance_id: String::new(),
            executable: PathBuf::new(),
            mount_point: PathBuf::from("/omnifs"),
            config_dir: PathBuf::from("/omnifs-home"),
            cache_dir: PathBuf::from("/omnifs-home/cache"),
            providers_dir: PathBuf::from("/omnifs-home/providers"),
            frontends: vec![
                frontend(FsType::Fuse, FrontendDelivery::Local, "/omnifs"),
                frontend(FsType::Fuse, FrontendDelivery::Docker, "/omnifs"),
            ],
            backend: DaemonBackend::default(),
            mounts: Vec::new(),
            failed: Vec::new(),
            health: DaemonHealth::default(),
        }
    }

    #[test]
    fn running_grid() {
        let report = StatusReport {
            paths: layout(),
            runtime: Some(running_runtime()),
            user_mounts: mounts(),
            providers: Vec::new(),
        };
        insta::assert_snapshot!(strip_ansi(&report.build_report(false).render()));
    }

    #[test]
    fn daemon_down_grid() {
        let report = StatusReport {
            paths: layout(),
            runtime: None,
            user_mounts: mounts(),
            providers: Vec::new(),
        };
        insta::assert_snapshot!(strip_ansi(&report.build_report(false).render()));
    }

    #[test]
    fn daemon_down_json() {
        let report = StatusReport {
            paths: layout(),
            runtime: None,
            user_mounts: mounts(),
            providers: Vec::new(),
        };
        insta::assert_snapshot!(serde_json::to_string_pretty(&report.into_json()).unwrap());
    }

    #[test]
    fn running_json_includes_frontend_details_without_changing_existing_fields() {
        let report = StatusReport {
            paths: layout(),
            runtime: Some(running_runtime()),
            user_mounts: mounts(),
            providers: Vec::new(),
        };
        insta::assert_snapshot!(serde_json::to_string_pretty(&report.into_json()).unwrap());
    }

    #[test]
    fn zero_frontends_are_explicit_and_uptime_is_not_inferred() {
        let mut runtime = running_runtime();
        runtime.frontends.clear();
        let report = StatusReport {
            paths: layout(),
            runtime: Some(runtime),
            user_mounts: Vec::new(),
            providers: Vec::new(),
        };
        let rendered = strip_ansi(&report.build_report(false).render());
        assert!(rendered.contains("Frontends (0)"));
        assert!(rendered.contains("no frontends attached"));
        assert!(!rendered.contains(" up "));
    }
}
