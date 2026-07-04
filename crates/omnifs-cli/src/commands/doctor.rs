//! `omnifs doctor` — environment + auth diagnostics. No auto-fix.

use clap::Args;
use std::fmt::Write as _;
use std::path::Path;

use omnifs_workspace::creds::FileStore;

use crate::auth::{AuthProbeSeverity, AuthProbeSummary};
use crate::launch_backend::{DockerTarget, ImageRef};
use crate::runtime::Runtime;
use crate::status::UserMountStatus;
use crate::workspace::Workspace;
use omnifs_workspace::layout::WorkspaceLayout;
use omnifs_workspace::provider::{Catalog, DirStatus};

#[derive(Args, Debug, Clone, Default)]
pub struct DoctorArgs {}

/// Aggregate result of a completed diagnostic run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DoctorVerdict {
    Clean,
    Warnings,
    Failures,
}

impl DoctorVerdict {
    fn record(&mut self, name: impl std::fmt::Display, result: &ProbeResult) {
        anstream::println!("{}", result.render(&name.to_string()));
        match result {
            ProbeResult::Err(_) => *self = Self::Failures,
            ProbeResult::Warn(_) if *self == Self::Clean => *self = Self::Warnings,
            ProbeResult::Ok(_) | ProbeResult::Warn(_) | ProbeResult::Skipped(_) => {},
        }
    }
}

impl DoctorArgs {
    pub async fn run(self) -> anyhow::Result<DoctorVerdict> {
        let workspace = Workspace::resolve()?;
        let mounts = workspace.mounts()?;
        let docker_target = workspace
            .config()
            .and_then(|config| DockerTarget::resolve(None, None, &config))
            .map_err(|error| format!("resolve target: {error:#}"));
        run(
            workspace.layout(),
            workspace.catalog(),
            mounts,
            docker_target,
        )
        .await
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

    fn render(&self, name: &str) -> String {
        format!("  {} {:<28} {}", self.glyph(), name, self.message())
    }
}

pub async fn run(
    paths: &WorkspaceLayout,
    catalog: &Catalog,
    mounts: Vec<crate::session::MountConfig>,
    docker_target: Result<DockerTarget, String>,
) -> anyhow::Result<DoctorVerdict> {
    let mut verdict = DoctorVerdict::Clean;

    // 1. docker_reachable
    let (runtime, docker_target, docker_result) = probe_docker_reachable(docker_target).await;
    let docker_ok = matches!(docker_result, ProbeResult::Ok(_));
    verdict.record("docker reachable", &docker_result);

    // 2. fuse (Linux only)
    verdict.record("fuse", &probe_fuse());

    // 3. image_cached (depends on docker)
    let image_result = match (docker_ok, runtime.as_ref(), docker_target.as_ref()) {
        (true, Some(runtime), Some(target)) => probe_image_cached(runtime, target.image()).await,
        _ => ProbeResult::Skipped("docker unreachable"),
    };
    verdict.record("image cached", &image_result);

    // 4. providers_discovered
    verdict.record("providers discovered", &probe_providers_discovered(catalog));

    // 5. credential store
    verdict.record("credential store", &probe_credential_store(paths));

    // 6. ssh_agent
    verdict.record("ssh-agent", &probe_ssh_agent());

    // 7. config file
    verdict.record("config file", &probe_config_file(paths));

    // 8. mount configs valid + 9. auth ready (combined because the loader does both)
    let mount_results = probe_mount_configs(paths, catalog, mounts);
    verdict.record("mount configs valid", &mount_results.0);
    for (mount, result) in mount_results.1 {
        verdict.record(format!("auth ready ({mount})"), &result);
    }

    // 10. network (best effort)
    verdict.record("network", &probe_network().await);

    Ok(verdict)
}

async fn probe_docker_reachable(
    target: Result<DockerTarget, String>,
) -> (Option<Runtime>, Option<DockerTarget>, ProbeResult) {
    use crate::runtime::DockerProbeOutcome;

    let target = match target {
        Ok(target) => target,
        Err(error) => return (None, None, ProbeResult::Err(error)),
    };

    match Runtime::probe_docker(&target).await {
        DockerProbeOutcome::Reachable(runtime) => (
            Some(runtime),
            Some(target),
            ProbeResult::Ok("docker daemon responds".into()),
        ),
        DockerProbeOutcome::ConnectFailed(e) => (
            None,
            Some(target),
            ProbeResult::Err(format!("connect: {e}")),
        ),
        DockerProbeOutcome::PingFailed(e) => {
            (None, Some(target), ProbeResult::Err(format!("ping: {e}")))
        },
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

async fn probe_image_cached(runtime: &Runtime, image: &ImageRef) -> ProbeResult {
    match runtime.inspect_image(image.as_str()).await {
        Ok(_) => ProbeResult::Ok(format!("{image} cached")),
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => ProbeResult::Warn(format!("{image} not cached (will pull on `omnifs up`)")),
        Err(error) => ProbeResult::Err(format!("inspect: {error}")),
    }
}

fn probe_providers_discovered(catalog: &Catalog) -> ProbeResult {
    match catalog.dir_status() {
        DirStatus::Present { wasm_count } if wasm_count > 0 => {
            ProbeResult::Ok(format!("{wasm_count} provider(s) installed"))
        },
        DirStatus::Missing | DirStatus::Present { .. } => {
            ProbeResult::Warn("no providers installed (run `omnifs up` or `omnifs setup`)".into())
        },
        DirStatus::Unreadable(error) => {
            ProbeResult::Err(format!("provider store unreadable: {error}"))
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
    catalog: &Catalog,
    mounts: Vec<crate::session::MountConfig>,
) -> (ProbeResult, Vec<(String, ProbeResult)>) {
    let store = FileStore::new(&paths.credentials_file);
    let mounts = crate::mount_report::scan_user_mount_configs(catalog, mounts, &store);
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
