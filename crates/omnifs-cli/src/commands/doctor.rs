//! `omnifs doctor` — environment + auth diagnostics. No auto-fix.

use clap::Args;
use omnifs_api::{CredentialHealth, CredentialStatus, DaemonStatus};
use serde::Serialize;
use std::fmt::Write as _;
use std::path::Path;

use omnifs_workspace::creds::FileStore;

use crate::auth::{AuthProbeSeverity, AuthProbeSummary};
use crate::cli::OutputFormat;
use crate::launch_backend::{DockerTarget, ImageRef};
use crate::runtime::Runtime;
use crate::status::UserMountStatus;
use crate::workspace::Workspace;
use omnifs_workspace::layout::WorkspaceLayout;
use omnifs_workspace::provider::{Catalog, DirStatus};

#[derive(Args, Debug, Clone, Default)]
pub struct DoctorArgs {
    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

/// Aggregate result of a completed diagnostic run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DoctorVerdict {
    Clean,
    Warnings,
    Failures,
}

impl DoctorVerdict {
    fn label(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Warnings => "warnings",
            Self::Failures => "failures",
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
        Doctor {
            workspace: &workspace,
            paths: workspace.layout(),
            catalog: workspace.catalog(),
            mounts,
            docker_target,
            output: OutputFormat::from(self.json),
        }
        .run()
        .await
    }
}

struct Doctor<'a> {
    workspace: &'a Workspace,
    paths: &'a WorkspaceLayout,
    catalog: &'a Catalog,
    mounts: Vec<crate::mount_config::MountConfig>,
    docker_target: Result<DockerTarget, String>,
    output: OutputFormat,
}

#[derive(Serialize)]
struct DoctorJson {
    verdict: &'static str,
    probes: Vec<ProbeJson>,
    live: LiveSection,
}

#[derive(Serialize)]
struct ProbeJson {
    name: String,
    state: &'static str,
    message: String,
}

#[derive(Default, Serialize)]
struct LiveSection {
    skipped: Option<String>,
    findings: Vec<LiveFinding>,
}

#[derive(Serialize)]
struct LiveFinding {
    mount: String,
    state: &'static str,
    message: String,
    fix: String,
}

struct DoctorReport {
    verdict: DoctorVerdict,
    probes: Vec<ProbeJson>,
    output: OutputFormat,
}

impl DoctorReport {
    fn new(output: OutputFormat) -> Self {
        Self {
            verdict: DoctorVerdict::Clean,
            probes: Vec::new(),
            output,
        }
    }

    fn record(&mut self, name: impl Into<String>, result: ProbeResult) {
        let name = name.into();
        if self.output == OutputFormat::Text {
            anstream::println!("{}", result.render(&name));
        }
        let (state, message) = match result {
            ProbeResult::Ok(message) => ("ok", message),
            ProbeResult::Warn(message) => {
                if self.verdict == DoctorVerdict::Clean {
                    self.verdict = DoctorVerdict::Warnings;
                }
                ("warn", message)
            },
            ProbeResult::Err(message) => {
                self.verdict = DoctorVerdict::Failures;
                ("err", message)
            },
            ProbeResult::Skipped(reason) => ("skipped", reason.to_string()),
        };
        self.probes.push(ProbeJson {
            name,
            state,
            message,
        });
    }

    fn record_live(&mut self, live: &LiveSection) {
        if live.skipped.is_some() {
            return;
        }
        if !live.findings.is_empty() && self.verdict == DoctorVerdict::Clean {
            self.verdict = DoctorVerdict::Warnings;
        }
    }

    fn finish(self, live: LiveSection) -> anyhow::Result<DoctorVerdict> {
        if self.output == OutputFormat::Json {
            let payload = DoctorJson {
                verdict: self.verdict.label(),
                probes: self.probes,
                live,
            };
            anstream::println!("{}", serde_json::to_string(&payload)?);
        }
        Ok(self.verdict)
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

impl Doctor<'_> {
    async fn run(self) -> anyhow::Result<DoctorVerdict> {
        let mut report = DoctorReport::new(self.output);

        let (runtime, docker_target, docker_result) = self.probe_docker_reachable().await;
        let docker_ok = matches!(docker_result, ProbeResult::Ok(_));
        report.record("docker reachable", docker_result);

        report.record("fuse", self.probe_fuse());

        let image_result = match (docker_ok, runtime.as_ref(), docker_target.as_ref()) {
            (true, Some(runtime), Some(target)) => {
                self.probe_image_cached(runtime, target.image()).await
            },
            _ => ProbeResult::Skipped("docker unreachable"),
        };
        report.record("image cached", image_result);

        report.record("providers discovered", self.probe_providers_discovered());
        report.record("credential store", self.probe_credential_store());
        report.record("ssh-agent", self.probe_ssh_agent());
        report.record("config file", self.probe_config_file());

        let mount_results = self.probe_mount_configs();
        report.record("mount configs valid", mount_results.0);
        for (mount, result) in mount_results.1 {
            report.record(format!("auth ready ({mount})"), result);
        }

        report.record("network", self.probe_network().await);

        let live = self.probe_live().await?;
        self.render_live(&live);
        report.record_live(&live);
        report.finish(live)
    }

    async fn probe_docker_reachable(&self) -> (Option<Runtime>, Option<DockerTarget>, ProbeResult) {
        use crate::runtime::DockerProbeOutcome;

        let target = match self.docker_target.clone() {
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

    #[allow(clippy::unused_self)] // Kept as a Doctor probe method for a uniform probe surface.
    fn probe_fuse(&self) -> ProbeResult {
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

    async fn probe_image_cached(&self, runtime: &Runtime, image: &ImageRef) -> ProbeResult {
        match runtime.inspect_image(image.as_str()).await {
            Ok(_) => ProbeResult::Ok(format!("{image} cached")),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => ProbeResult::Warn(format!("{image} not cached (will pull on `omnifs up`)")),
            Err(error) => ProbeResult::Err(format!("inspect: {error}")),
        }
    }

    fn probe_providers_discovered(&self) -> ProbeResult {
        match self.catalog.dir_status() {
            DirStatus::Present { wasm_count } if wasm_count > 0 => {
                ProbeResult::Ok(format!("{wasm_count} provider(s) installed"))
            },
            DirStatus::Missing | DirStatus::Present { .. } => ProbeResult::Warn(
                "no providers installed (run `omnifs up` or `omnifs setup`)".into(),
            ),
            DirStatus::Unreadable(error) => {
                ProbeResult::Err(format!("provider store unreadable: {error}"))
            },
        }
    }

    fn probe_credential_store(&self) -> ProbeResult {
        let Some(parent) = self.paths.credentials_file.parent() else {
            return ProbeResult::Err(format!(
                "credential file has no parent: {}",
                self.paths.credentials_file.display()
            ));
        };
        if parent.exists() {
            ProbeResult::Ok(format!(
                "file {}",
                WorkspaceLayout::display(&self.paths.credentials_file)
            ))
        } else {
            ProbeResult::Warn(format!(
                "credential directory will be created on first write: {}",
                WorkspaceLayout::display(parent)
            ))
        }
    }

    #[allow(clippy::unused_self)] // Kept as a Doctor probe method for a uniform probe surface.
    fn probe_ssh_agent(&self) -> ProbeResult {
        match std::env::var_os("SSH_AUTH_SOCK") {
            Some(sock) if Path::new(&sock).exists() => {
                ProbeResult::Ok(WorkspaceLayout::display(Path::new(&sock)))
            },
            Some(_) => ProbeResult::Warn("SSH_AUTH_SOCK set but socket not found".into()),
            None => ProbeResult::Warn("SSH_AUTH_SOCK unset; git callouts will fail".into()),
        }
    }

    fn probe_config_file(&self) -> ProbeResult {
        if self.paths.config_file.exists() {
            ProbeResult::Ok(WorkspaceLayout::display(&self.paths.config_file))
        } else {
            ProbeResult::Ok(format!(
                "(default; {} absent)",
                WorkspaceLayout::display(&self.paths.config_file)
            ))
        }
    }

    #[allow(clippy::unused_self)] // Kept as a Doctor probe method for a uniform probe surface.
    fn probe_result_from_summary(&self, summary: AuthProbeSummary) -> ProbeResult {
        match summary.severity {
            AuthProbeSeverity::Ok => ProbeResult::Ok(summary.message),
            AuthProbeSeverity::Warn => ProbeResult::Warn(summary.message),
            AuthProbeSeverity::Err => ProbeResult::Err(summary.message),
        }
    }

    fn probe_mount_configs(&self) -> (ProbeResult, Vec<(String, ProbeResult)>) {
        let store = FileStore::new(&self.paths.credentials_file);
        let mounts =
            crate::mount_report::scan_user_mount_configs(self.catalog, self.mounts.clone(), &store);
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
                let result = self.probe_result_from_summary(mount.auth.probe_summary());
                Some((mount.mount.clone(), result))
            })
            .collect();

        (configs_result, auth_results)
    }

    async fn probe_network(&self) -> ProbeResult {
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

    async fn probe_live(&self) -> anyhow::Result<LiveSection> {
        if !self.workspace.daemon().ready().await {
            return Ok(LiveSection {
                skipped: Some("no ready compatible daemon answered".to_string()),
                findings: Vec::new(),
            });
        }
        let Some(status) = self.workspace.daemon().compatible_status_optional().await? else {
            return Ok(LiveSection {
                skipped: Some("no compatible daemon answered".to_string()),
                findings: Vec::new(),
            });
        };
        let credentials = self
            .workspace
            .daemon()
            .credentials_if_ready()
            .await?
            .unwrap_or_default();
        let provider_by_mount: std::collections::BTreeMap<String, String> = self
            .workspace
            .mounts()
            .unwrap_or_default()
            .into_iter()
            .map(|mount| {
                let provider = mount.config.provider_name().to_string();
                (mount.name.to_string(), provider)
            })
            .collect();
        Ok(LiveSection {
            skipped: None,
            findings: live_findings(&status, &credentials, &provider_by_mount),
        })
    }

    fn render_live(&self, live: &LiveSection) {
        if self.output == OutputFormat::Json {
            return;
        }
        anstream::println!();
        anstream::println!("Live daemon");
        if let Some(reason) = &live.skipped {
            anstream::println!("  · skipped: {reason}");
            return;
        }
        if live.findings.is_empty() {
            anstream::println!("  ✓ all live mounts are healthy");
            return;
        }
        for finding in &live.findings {
            let glyph = match finding.state {
                "err" => "✗",
                _ => "⚠",
            };
            anstream::println!(
                "  {glyph} {:<14} {}; fix: {}",
                finding.mount,
                finding.message,
                finding.fix
            );
        }
    }
}

fn live_findings(
    status: &DaemonStatus,
    credentials: &[CredentialStatus],
    provider_by_mount: &std::collections::BTreeMap<String, String>,
) -> Vec<LiveFinding> {
    let mut findings = Vec::new();
    for failure in &status.failed {
        // A load failure is not necessarily an auth problem (capability
        // under-grants land here too), so the fix is spec recreation, which is
        // also what the daemon's own error text recommends.
        let fix = match provider_by_mount.get(&failure.mount) {
            Some(provider) => format!("omnifs init {provider} --as {}", failure.mount),
            None => "omnifs logs".to_string(),
        };
        findings.push(LiveFinding {
            mount: failure.mount.clone(),
            state: "err",
            message: format!("failed to load: {}", failure.reason),
            fix,
        });
    }
    for mount in &status.mounts {
        let mut credential_findings = credentials
            .iter()
            .filter(|credential| {
                credential
                    .id
                    .starts_with(&format!("{}:", mount.provider_name))
                    && credential.health != CredentialHealth::Ready
            })
            .map(|credential| LiveFinding {
                mount: mount.mount.clone(),
                state: "warn",
                message: format!(
                    "credential `{}` is {}",
                    credential.id,
                    credential_health_label(credential.health)
                ),
                fix: format!("omnifs mounts reauth {}", mount.mount),
            })
            .peekable();
        // The aggregate mount health is derived from the same credentials, so
        // only fall back to it when no per-credential finding names the cause.
        if credential_findings.peek().is_some() {
            findings.extend(credential_findings);
        } else if let Some(health) = mount.auth_health
            && health != CredentialHealth::Ready
        {
            findings.push(LiveFinding {
                mount: mount.mount.clone(),
                state: "warn",
                message: format!("credential health is {}", credential_health_label(health)),
                fix: format!("omnifs mounts reauth {}", mount.mount),
            });
        }
    }
    findings
}

fn credential_health_label(health: CredentialHealth) -> &'static str {
    match health {
        CredentialHealth::Ready => "ready",
        CredentialHealth::ExpiringSoon => "expiring soon",
        CredentialHealth::Expired => "expired",
        CredentialHealth::RefreshFailed => "refresh failed",
        CredentialHealth::NeedsConsent => "needs consent",
        CredentialHealth::Missing => "missing",
        CredentialHealth::StaticUnvalidated => "static unvalidated",
    }
}
