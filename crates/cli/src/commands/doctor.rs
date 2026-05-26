//! `omnifs doctor` — environment + auth diagnostics. No auto-fix.

use bollard::Docker;
use clap::Args;
use omnifs_creds::KeyringStore;
use std::borrow::Cow;
use std::fmt::Write as _;
use std::path::Path;

use crate::app_context::AppContext;
use crate::auth::{AuthProbeSeverity, AuthProbeSummary};
use crate::catalog::{ProviderCatalog, ProviderDirStatus};
use crate::paths::Paths;
use crate::status::UserMountStatus;

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
        let ctx = AppContext::resolve_default()?;
        run(ctx.paths(), ctx.catalog()).await
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

pub async fn run(paths: &Paths, catalog: &ProviderCatalog) -> anyhow::Result<DoctorVerdict> {
    let mut report = DoctorReport::default();

    // 1. docker_reachable
    let (docker, docker_result) = probe_docker_reachable().await;
    let docker_ok = matches!(docker_result, ProbeResult::Ok(_));
    anstream::println!("{}", report.record("docker reachable", docker_result));

    // 2. fuse (Linux only)
    let fuse_result = probe_fuse();
    anstream::println!("{}", report.record("fuse", fuse_result));

    // 3. image_cached (depends on docker)
    let image_result = match (docker_ok, docker.as_ref()) {
        (true, Some(docker)) => probe_image_cached(docker).await,
        _ => ProbeResult::Skipped("docker unreachable"),
    };
    anstream::println!("{}", report.record("image cached", image_result));

    // 4. providers_discovered
    let providers_result = probe_providers_discovered(catalog);
    anstream::println!(
        "{}",
        report.record("providers discovered", providers_result)
    );

    // 5. keychain backend
    let keychain_result = probe_keychain_backend(paths);
    anstream::println!("{}", report.record("keychain backend", keychain_result));

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

async fn probe_image_cached(docker: &Docker) -> ProbeResult {
    match docker.inspect_image(IMAGE).await {
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

fn probe_keychain_backend(paths: &Paths) -> ProbeResult {
    match KeyringStore::new() {
        Ok(_) => ProbeResult::Ok("keychain available".into()),
        Err(error) => ProbeResult::Warn(format!(
            "keychain unavailable ({error}); fallback to {}",
            Paths::display(&paths.credentials_file)
        )),
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
    let store = crate::session::open_store(&paths.credentials_file, false);
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
