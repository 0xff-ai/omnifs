//! `omnifs doctor` — environment + auth diagnostics. No auto-fix.

use clap::Args;
use std::borrow::Cow;
use std::fmt::Write as _;
use std::path::Path;

use omnifs_creds::FileStore;

use crate::auth::{AuthProbeSeverity, AuthProbeSummary};
use crate::catalog::{ProviderCatalog, ProviderDirStatus};
use crate::runtime::Runtime;
use crate::status::UserMountStatus;
use crate::workspace::Workspace;
use omnifs_home::WorkspaceLayout;

const IMAGE: &str = concat!("ghcr.io/0xff-ai/omnifs:", env!("CARGO_PKG_VERSION"));

#[derive(Args, Debug, Clone, Default)]
pub struct DoctorArgs {}

/// Aggregate result of a completed diagnostic run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DoctorVerdict {
    Clean,
    Warnings,
    Failures,
}

impl DoctorArgs {
    pub async fn run(self) -> anyhow::Result<DoctorVerdict> {
        let workspace = Workspace::resolve()?;
        let mounts = workspace.mounts()?;
        run(workspace.layout(), workspace.catalog(), mounts).await
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
    paths: &WorkspaceLayout,
    catalog: &ProviderCatalog,
    mounts: Vec<crate::session::MountConfig>,
) -> anyhow::Result<DoctorVerdict> {
    let mut report = DoctorReport::default();

    // 1. docker_reachable
    let (runtime, docker_result) = probe_docker_reachable().await;
    let docker_ok = matches!(docker_result, ProbeResult::Ok(_));
    anstream::println!("{}", report.record("docker reachable", docker_result));

    // 2. fuse (Linux only)
    let fuse_result = probe_fuse();
    anstream::println!("{}", report.record("fuse", fuse_result));

    // 3. image_cached (depends on docker)
    let image_result = match (docker_ok, runtime.as_ref()) {
        (true, Some(runtime)) => probe_image_cached(runtime).await,
        _ => ProbeResult::Skipped("docker unreachable"),
    };
    anstream::println!("{}", report.record("image cached", image_result));

    // 4. providers_discovered
    let providers_result = probe_providers_discovered(catalog);
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
    let mount_results = probe_mount_configs(paths, catalog, mounts);
    anstream::println!("{}", report.record("mount configs valid", mount_results.0));
    for (mount, result) in mount_results.1 {
        anstream::println!("{}", report.record(format!("auth ready ({mount})"), result));
    }

    // 10. network (best effort)
    let network_result = probe_network().await;
    anstream::println!("{}", report.record("network", network_result));

    Ok(report.verdict())
}

async fn probe_docker_reachable() -> (Option<Runtime>, ProbeResult) {
    use crate::launch_backend::DockerTarget;
    use crate::runtime::DockerProbeOutcome;

    // Use the default runtime target so that probe_image_cached checks the
    // same image omnifs up would pull.
    let target = match Workspace::resolve().and_then(|workspace| {
        let config = workspace.config()?;
        DockerTarget::resolve(None, None, &config)
    }) {
        Ok(target) => target,
        Err(error) => return (None, ProbeResult::Err(format!("resolve target: {error}"))),
    };

    match Runtime::probe_docker(&target).await {
        DockerProbeOutcome::Reachable(runtime) => (
            Some(runtime),
            ProbeResult::Ok("docker daemon responds".into()),
        ),
        DockerProbeOutcome::ConnectFailed(e) => (None, ProbeResult::Err(format!("connect: {e}"))),
        DockerProbeOutcome::PingFailed(e) => (None, ProbeResult::Err(format!("ping: {e}"))),
    }
}

fn probe_fuse() -> ProbeResult {
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
        ProbeResult::Skipped("macOS: containerized FUSE")
    }
}

async fn probe_image_cached(runtime: &Runtime) -> ProbeResult {
    match runtime.inspect_image(IMAGE).await {
        Ok(_) => ProbeResult::Ok(format!("{IMAGE} cached")),
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => ProbeResult::Warn(format!("{IMAGE} not cached (will pull on `omnifs up`)")),
        Err(error) => ProbeResult::Err(format!("inspect: {error}")),
    }
}

fn probe_providers_discovered(catalog: &ProviderCatalog) -> ProbeResult {
    let builtin_count = ProviderCatalog::builtin_manifests()
        .map(|v| v.len())
        .unwrap_or(0);
    match catalog.provider_dir_status() {
        ProviderDirStatus::Missing if builtin_count > 0 => {
            ProbeResult::Ok(format!("{builtin_count} built-in, provider dir missing"))
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

fn probe_credential_store(paths: &WorkspaceLayout) -> ProbeResult {
    let Some(parent) = paths.credentials_file.parent() else {
        return ProbeResult::Err(format!(
            "credential file has no parent: {}",
            paths.credentials_file.display()
        ));
    };
    if parent.exists() {
        ProbeResult::Ok(format!(
            "file {}",
            WorkspaceLayout::display(&paths.credentials_file)
        ))
    } else {
        ProbeResult::Warn(format!(
            "credential directory will be created on first write: {}",
            WorkspaceLayout::display(parent)
        ))
    }
}

fn probe_ssh_agent() -> ProbeResult {
    match std::env::var_os("SSH_AUTH_SOCK") {
        Some(sock) if Path::new(&sock).exists() => {
            ProbeResult::Ok(WorkspaceLayout::display(Path::new(&sock)))
        },
        Some(_) => ProbeResult::Warn("SSH_AUTH_SOCK set but socket not found".into()),
        None => ProbeResult::Warn("SSH_AUTH_SOCK unset; git callouts will fail".into()),
    }
}

fn probe_config_file(paths: &WorkspaceLayout) -> ProbeResult {
    if paths.config_file.exists() {
        ProbeResult::Ok(WorkspaceLayout::display(&paths.config_file))
    } else {
        ProbeResult::Ok(format!(
            "(default; {} absent)",
            WorkspaceLayout::display(&paths.config_file)
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
    paths: &WorkspaceLayout,
    catalog: &ProviderCatalog,
    mounts: Vec<crate::session::MountConfig>,
) -> (ProbeResult, Vec<(String, ProbeResult)>) {
    let store = FileStore::new(&paths.credentials_file);
    let mounts = catalog.scan_user_mount_configs(mounts, &store);
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
