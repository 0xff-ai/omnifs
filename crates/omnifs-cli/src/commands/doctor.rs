//! `omnifs doctor` — environment + auth diagnostics. No auto-fix.

use bollard::Docker;
use clap::Args;
use std::borrow::Cow;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::Command;

use crate::app_context::AppContext;
use crate::auth::{AuthProbeSeverity, AuthProbeSummary};
use crate::catalog::{ProviderCatalog, ProviderDirStatus};
use crate::paths::{PathOverrides, Paths};
use crate::runtime_mode::RuntimeMode;
use crate::runtime_target::RuntimeTarget;
use crate::session::CredsBackend;
use crate::status::UserMountStatus;

const IMAGE: &str = concat!("ghcr.io/0xff-ai/omnifs:", env!("CARGO_PKG_VERSION"));

#[derive(Args, Debug, Clone, Default)]
pub struct DoctorArgs {
    /// Runtime mode to diagnose.
    #[arg(long, value_enum)]
    pub mode: Option<RuntimeMode>,
    /// Host mount point for native mode.
    #[arg(long)]
    pub mount_point: Option<PathBuf>,
}

/// Aggregate result of a completed diagnostic run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DoctorVerdict {
    Clean,
    Warnings,
    Failures,
}

impl DoctorArgs {
    pub async fn run(self) -> anyhow::Result<DoctorVerdict> {
        let ctx = AppContext::resolve_with_runtime(
            PathOverrides::default(),
            None,
            None,
            self.mode,
            self.mount_point,
        )?;
        run(ctx.paths(), ctx.catalog(), ctx.runtime()).await
    }
}

#[derive(Debug)]
enum ProbeResult {
    Ok(String),
    Warn(String),
    Err(String),
    Skipped(&'static str),
}

impl ProbeResult {
    fn glyph(&self) -> &'static str {
        match self {
            Self::Ok(_) => "✓",
            Self::Warn(_) => "⚠",
            Self::Err(_) => "✗",
            Self::Skipped(_) => "·",
        }
    }

    fn message(&self) -> &str {
        match self {
            Self::Ok(m) | Self::Warn(m) | Self::Err(m) => m.as_str(),
            Self::Skipped(reason) => reason,
        }
    }
}

#[derive(Debug)]
struct ProbeRecord {
    name: Cow<'static, str>,
    result: ProbeResult,
}

impl ProbeRecord {
    fn render(&self) -> String {
        format!(
            "  {} {:<28} {}",
            self.result.glyph(),
            self.name,
            self.result.message()
        )
    }
}

#[derive(Debug, Default)]
struct DoctorReport {
    probes: Vec<ProbeRecord>,
}

impl DoctorReport {
    fn record(&mut self, name: impl Into<Cow<'static, str>>, result: ProbeResult) -> String {
        self.probes.push(ProbeRecord {
            name: name.into(),
            result,
        });
        self.probes.last().expect("record just pushed").render()
    }

    fn verdict(&self) -> DoctorVerdict {
        let any_red = self
            .probes
            .iter()
            .any(|probe| matches!(probe.result, ProbeResult::Err(_)));
        let any_yellow = self
            .probes
            .iter()
            .any(|probe| matches!(probe.result, ProbeResult::Warn(_)));
        if any_red {
            DoctorVerdict::Failures
        } else if any_yellow {
            DoctorVerdict::Warnings
        } else {
            DoctorVerdict::Clean
        }
    }
}

pub async fn run(
    paths: &Paths,
    catalog: &ProviderCatalog,
    runtime: &RuntimeTarget,
) -> anyhow::Result<DoctorVerdict> {
    let mut report = DoctorReport::default();

    anstream::println!(
        "{}",
        report.record(
            "runtime mode",
            ProbeResult::Ok(match runtime {
                RuntimeTarget::Docker(_) => "docker".into(),
                RuntimeTarget::Native(target) => {
                    format!("native ({})", target.mount_point().display())
                },
            }),
        )
    );

    let docker = match runtime {
        RuntimeTarget::Docker(_) => {
            let (docker, docker_result) = probe_docker_reachable().await;
            anstream::println!("{}", report.record("docker reachable", docker_result));
            docker
        },
        RuntimeTarget::Native(_) => {
            anstream::println!(
                "{}",
                report.record("docker reachable", ProbeResult::Skipped("native mode"))
            );
            None
        },
    };

    let frontend_result = match runtime {
        RuntimeTarget::Docker(_) => probe_fuse_for_docker(),
        RuntimeTarget::Native(_) => probe_native_frontend(),
    };
    anstream::println!("{}", report.record("filesystem frontend", frontend_result));

    let image_result = match (runtime, docker.as_ref()) {
        (RuntimeTarget::Docker(_), Some(docker)) => probe_image_cached(docker).await,
        (RuntimeTarget::Docker(_), None) => ProbeResult::Skipped("docker unreachable"),
        (RuntimeTarget::Native(_), _) => ProbeResult::Skipped("native mode"),
    };
    anstream::println!("{}", report.record("image cached", image_result));

    let providers_result =
        probe_providers_discovered(catalog, matches!(runtime, RuntimeTarget::Native(_)));
    anstream::println!(
        "{}",
        report.record("providers discovered", providers_result)
    );

    // 5. credential store
    let credential_store_result = probe_credential_store(paths);
    anstream::println!(
        "{}",
        report.record("credential store", credential_store_result)
    );

    // 6. ssh_agent
    let ssh_result = probe_ssh_agent();
    anstream::println!("{}", report.record("ssh-agent", ssh_result));

    // 7. config file
    let cfg_result = probe_config_file(paths);
    anstream::println!("{}", report.record("config file", cfg_result));

    // 8. mount configs valid + 9. auth ready (combined because the loader does both)
    let mount_results = probe_mount_configs(paths, catalog);
    anstream::println!("{}", report.record("mount configs valid", mount_results.0));
    for (mount, result) in mount_results.1 {
        anstream::println!("{}", report.record(format!("auth ready ({mount})"), result));
    }

    // 10. network (best effort)
    let network_result = probe_network().await;
    anstream::println!("{}", report.record("network", network_result));

    Ok(report.verdict())
}

async fn probe_docker_reachable() -> (Option<Docker>, ProbeResult) {
    let docker = match Docker::connect_with_local_defaults() {
        Ok(d) => d,
        Err(error) => return (None, ProbeResult::Err(format!("connect: {error}"))),
    };
    match docker.ping().await {
        Ok(_) => (
            Some(docker),
            ProbeResult::Ok("docker daemon responds".into()),
        ),
        Err(error) => (None, ProbeResult::Err(format!("ping: {error}"))),
    }
}

fn probe_fuse_for_docker() -> ProbeResult {
    #[cfg(target_os = "linux")]
    {
        let path = Path::new("/dev/fuse");
        if !path.exists() {
            return ProbeResult::Err("/dev/fuse does not exist".into());
        }
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
        {
            Ok(_) => ProbeResult::Ok("/dev/fuse openable".into()),
            Err(error) => ProbeResult::Err(format!("/dev/fuse open: {error}")),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        ProbeResult::Skipped("docker provides Linux FUSE")
    }
}

fn probe_native_frontend() -> ProbeResult {
    #[cfg(target_os = "macos")]
    {
        if Path::new("/sbin/mount_nfs").exists() || command_exists("mount_nfs") {
            ProbeResult::Ok("NFSv4 loopback helper available".into())
        } else {
            ProbeResult::Err("mount_nfs not found".into())
        }
    }
    #[cfg(target_os = "linux")]
    {
        probe_linux_fuse()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        ProbeResult::Err("native mode is supported on macOS and Linux only".into())
    }
}

#[cfg(target_os = "linux")]
fn probe_linux_fuse() -> ProbeResult {
    let path = Path::new("/dev/fuse");
    if !path.exists() {
        return ProbeResult::Err("/dev/fuse does not exist".into());
    }
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    {
        Ok(_) => ProbeResult::Ok("/dev/fuse openable".into()),
        Err(error) => ProbeResult::Err(format!("/dev/fuse open: {error}")),
    }
}

#[cfg(target_os = "macos")]
fn command_exists(command: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {command} >/dev/null 2>&1"))
        .status()
        .is_ok_and(|status| status.success())
}

async fn probe_image_cached(docker: &Docker) -> ProbeResult {
    match docker.inspect_image(IMAGE).await {
        Ok(_) => ProbeResult::Ok(format!("{IMAGE} cached")),
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => ProbeResult::Warn(format!("{IMAGE} not cached (will pull on `omnifs up`)")),
        Err(error) => ProbeResult::Err(format!("inspect: {error}")),
    }
}

fn probe_providers_discovered(catalog: &ProviderCatalog, native: bool) -> ProbeResult {
    let builtin_count = ProviderCatalog::builtin_manifests()
        .map(|v| v.len())
        .unwrap_or(0);
    match catalog.provider_dir_status() {
        ProviderDirStatus::Missing if native => {
            ProbeResult::Err("native mode needs provider .wasm files on disk".into())
        },
        ProviderDirStatus::Missing if builtin_count > 0 => {
            ProbeResult::Ok(format!("{builtin_count} built-in, provider dir missing"))
        },
        ProviderDirStatus::Present { wasm_count } if native && wasm_count == 0 => {
            ProbeResult::Err("native mode needs provider .wasm files on disk".into())
        },
        ProviderDirStatus::Present { wasm_count } if builtin_count + wasm_count > 0 => {
            ProbeResult::Ok(format!("{builtin_count} built-in, {wasm_count} on disk"))
        },
        ProviderDirStatus::Missing | ProviderDirStatus::Present { .. } => {
            ProbeResult::Warn("no providers discovered".into())
        },
        ProviderDirStatus::Unreadable(error) => {
            ProbeResult::Err(format!("provider dir unreadable: {error}"))
        },
    }
}

fn probe_credential_store(paths: &Paths) -> ProbeResult {
    let Some(parent) = paths.credentials_file.parent() else {
        return ProbeResult::Err(format!(
            "credential file has no parent: {}",
            paths.credentials_file.display()
        ));
    };
    if parent.exists() {
        ProbeResult::Ok(format!("file {}", Paths::display(&paths.credentials_file)))
    } else {
        ProbeResult::Warn(format!(
            "credential directory will be created on first write: {}",
            Paths::display(parent)
        ))
    }
}

fn probe_ssh_agent() -> ProbeResult {
    match std::env::var_os("SSH_AUTH_SOCK") {
        Some(sock) if Path::new(&sock).exists() => {
            ProbeResult::Ok(Paths::display(Path::new(&sock)))
        },
        Some(_) => ProbeResult::Warn("SSH_AUTH_SOCK set but socket not found".into()),
        None => ProbeResult::Warn("SSH_AUTH_SOCK unset; git callouts will fail".into()),
    }
}

fn probe_config_file(paths: &Paths) -> ProbeResult {
    if paths.config_file.exists() {
        ProbeResult::Ok(Paths::display(&paths.config_file))
    } else {
        ProbeResult::Ok(format!(
            "(default; {} absent)",
            Paths::display(&paths.config_file)
        ))
    }
}

fn probe_result_from_summary(summary: AuthProbeSummary) -> ProbeResult {
    match summary.severity {
        AuthProbeSeverity::Ok => ProbeResult::Ok(summary.message),
        AuthProbeSeverity::Warn => ProbeResult::Warn(summary.message),
        AuthProbeSeverity::Err => ProbeResult::Err(summary.message),
    }
}

fn probe_mount_configs(
    paths: &Paths,
    catalog: &ProviderCatalog,
) -> (ProbeResult, Vec<(String, ProbeResult)>) {
    let store = CredsBackend::auto(&paths.credentials_file, false);
    let mounts = match catalog.scan_user_mount_configs(store.as_ref()) {
        Ok(m) => m,
        Err(error) => {
            return (ProbeResult::Err(format!("scan: {error}")), Vec::new());
        },
    };
    let invalid: Vec<_> = mounts
        .iter()
        .filter_map(|m| match m {
            UserMountStatus::Invalid { config_path, error } => Some((
                config_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("<unknown>")
                    .to_string(),
                error.clone(),
            )),
            UserMountStatus::Ready(_) => None,
        })
        .collect();
    let valid_count = mounts.len() - invalid.len();

    let configs_result = if invalid.is_empty() {
        ProbeResult::Ok(format!("{valid_count} mount(s) valid"))
    } else {
        let mut msg = String::new();
        let _ = write!(
            &mut msg,
            "{} valid, {} invalid:",
            valid_count,
            invalid.len()
        );
        for (name, error) in &invalid {
            let _ = write!(&mut msg, "\n      - {name}: {error}");
        }
        ProbeResult::Err(msg)
    };

    let auth_results = mounts
        .iter()
        .filter_map(|mount| {
            let UserMountStatus::Ready(mount) = mount else {
                return None;
            };
            let result = probe_result_from_summary(mount.auth.probe_summary());
            Some((mount.mount.clone(), result))
        })
        .collect();

    (configs_result, auth_results)
}

async fn probe_network() -> ProbeResult {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
    {
        Ok(c) => c,
        Err(error) => return ProbeResult::Warn(format!("client build: {error}")),
    };
    match client.head("https://ghcr.io").send().await {
        Ok(_) => ProbeResult::Ok("ghcr.io reachable".into()),
        Err(error) => ProbeResult::Warn(format!("ghcr.io unreachable: {error}")),
    }
}
