//! `omnifs doctor` — environment + auth diagnostics. No auto-fix.

use clap::Args;
use omnifs_api::{CredentialHealth, CredentialStatus, DaemonStatus};
use serde::Serialize;
use std::path::Path;

use omnifs_workspace::creds::FileStore;

use crate::auth::{AuthProbeSeverity, AuthProbeSummary};
use crate::cli::OutputFormat;
use crate::frontend_container::{frontend_container_name, resolve_frontend_image};
use crate::launch_backend::{DockerTarget, ImageRef, names_registry};
use crate::runtime::Runtime;
use crate::status::UserMountStatus;
use crate::ui::report::{Report, Row, Section};
use crate::ui::style::Glyph;
use crate::workspace::Workspace;
use omnifs_workspace::layout::WorkspaceLayout;
use omnifs_workspace::provider::DirStatus;

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
        let docker_target = resolve_frontend_target(&workspace)
            .map_err(|error: anyhow::Error| format!("resolve target: {error:#}"));
        Doctor {
            workspace: &workspace,
            mounts,
            docker_target,
            output: OutputFormat::from(self.json),
        }
        .run()
        .await
    }
}

/// The optional Docker-hosted FUSE frontend's target, probed by the
/// `docker reachable`/`image cached` diagnostics. The daemon itself always
/// runs host-native, so there is no daemon Docker target to resolve here.
fn resolve_frontend_target(workspace: &Workspace) -> anyhow::Result<DockerTarget> {
    let config = workspace.config()?;
    let paths = workspace.layout();
    let image = resolve_frontend_image(None, &config)?;
    let container_name = frontend_container_name(paths)?;
    DockerTarget::new(
        container_name.as_str().to_string(),
        image.as_str().to_string(),
    )
}

struct Doctor<'a> {
    workspace: &'a Workspace,
    mounts: Vec<crate::mount_config::MountConfig>,
    /// The frontend's Docker target, or the error resolving it.
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
    /// The human grid rows, collected in lockstep with `probes` so the same
    /// facts drive the text report and the JSON.
    rows: Vec<Row>,
    output: OutputFormat,
}

impl DoctorReport {
    fn new(output: OutputFormat) -> Self {
        Self {
            verdict: DoctorVerdict::Clean,
            probes: Vec::new(),
            rows: Vec::new(),
            output,
        }
    }

    fn record(&mut self, name: impl Into<String>, result: ProbeResult) {
        let name = name.into();
        self.rows.push(Row::new(
            result.glyph(),
            name.clone(),
            result.message().to_string(),
        ));
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
        if live.findings.iter().any(|finding| finding.state == "err") {
            self.verdict = DoctorVerdict::Failures;
        } else if !live.findings.is_empty() && self.verdict == DoctorVerdict::Clean {
            self.verdict = DoctorVerdict::Warnings;
        }
    }

    fn finish(self, live: LiveSection, paths: Section) -> anyhow::Result<DoctorVerdict> {
        match self.output {
            OutputFormat::Json => {
                crate::ui::print_json(&DoctorJson {
                    verdict: self.verdict.label(),
                    probes: self.probes,
                    live,
                })?;
            },
            OutputFormat::Text => {
                let failures = self.count("err") + live_count(&live, "err");
                let warnings = self.count("warn") + live_count(&live, "warn");
                let mut report = Report::new();
                let mut diagnostics = Section::new("Diagnostics").counted(self.probes.len());
                for row in self.rows {
                    diagnostics.push(row);
                }
                report.push(diagnostics);
                report.push(live_section(&live));
                // The workspace paths block lives here, not on `version`: it is
                // diagnostic detail, and doctor ends with its verdict line.
                report.push(paths);
                report.push(Section::new(verdict_line(self.verdict, warnings, failures)));
                report.print();
            },
        }
        Ok(self.verdict)
    }

    fn count(&self, state: &str) -> usize {
        self.probes
            .iter()
            .filter(|probe| probe.state == state)
            .count()
    }
}

/// Count live findings whose state matches (`"err"` for failures, anything else
/// for warnings). A skipped live section has no findings.
fn live_count(live: &LiveSection, state: &str) -> usize {
    live.findings
        .iter()
        .filter(|finding| {
            if state == "err" {
                finding.state == "err"
            } else {
                finding.state != "err"
            }
        })
        .count()
}

/// The `Live daemon` section: a skip row when no daemon answered, a single
/// healthy row when every mount is fine, or one row per finding with its fix.
fn live_section(live: &LiveSection) -> Section {
    let mut section = Section::new("Live daemon");
    if let Some(reason) = &live.skipped {
        section.push(Row::new(Glyph::Skip, "", format!("skipped: {reason}")));
    } else if live.findings.is_empty() {
        section.push(Row::new(Glyph::Done, "", "all live mounts are healthy"));
    } else {
        for finding in &live.findings {
            let glyph = if finding.state == "err" {
                Glyph::Fail
            } else {
                Glyph::Warn
            };
            section.push(
                Row::new(
                    glyph,
                    finding.mount.clone(),
                    format!("{}; run `{}`", finding.message, finding.fix),
                )
                .identity()
                .with_fix(finding.fix.clone()),
            );
        }
    }
    section
}

/// The closing verdict line, pluralized: `verdict: clean`, `verdict: 1 warning`,
/// `verdict: 2 failures`.
fn verdict_line(verdict: DoctorVerdict, warnings: usize, failures: usize) -> String {
    match verdict {
        DoctorVerdict::Clean => "verdict: clean".to_string(),
        DoctorVerdict::Warnings => format!("verdict: {warnings} {}", plural(warnings, "warning")),
        DoctorVerdict::Failures => format!("verdict: {failures} {}", plural(failures, "failure")),
    }
}

fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        word.to_string()
    } else {
        format!("{word}s")
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
    fn from_auth_summary(summary: AuthProbeSummary) -> Self {
        match summary.severity {
            AuthProbeSeverity::Ok => Self::Ok(summary.message),
            AuthProbeSeverity::Warn => Self::Warn(summary.message),
            AuthProbeSeverity::Err => Self::Err(summary.message),
        }
    }

    /// The closed-vocabulary glyph for this probe outcome.
    fn glyph(&self) -> Glyph {
        match self {
            Self::Ok(_) => Glyph::Done,
            Self::Warn(_) => Glyph::Warn,
            Self::Err(_) => Glyph::Fail,
            Self::Skipped(_) => Glyph::Skip,
        }
    }

    fn message(&self) -> &str {
        match self {
            Self::Ok(m) | Self::Warn(m) | Self::Err(m) => m.as_str(),
            Self::Skipped(reason) => reason,
        }
    }
}

impl Doctor<'_> {
    async fn run(self) -> anyhow::Result<DoctorVerdict> {
        let mut report = DoctorReport::new(self.output);

        let (runtime, docker_result) = self.probe_docker_reachable().await;
        let docker_ok = matches!(docker_result, ProbeResult::Ok(_));
        report.record("docker reachable", docker_result);

        report.record("fuse", self.probe_fuse());

        let image_result = match (
            docker_ok,
            runtime.as_ref(),
            self.docker_target.as_ref().ok(),
        ) {
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
        report.record_live(&live);
        let paths = self.paths_section();
        report.finish(live, paths)
    }

    /// The workspace paths block: config, cache, mounts, providers, credentials,
    /// and the config file, each on its own informational row. Moved here from
    /// `version --detail`; it is diagnostic detail, not a version fact.
    fn paths_section(&self) -> Section {
        let layout = self.workspace.layout();
        let mut section = Section::new("Paths");
        for (key, path) in [
            ("config", &layout.config_dir),
            ("cache", &layout.cache_dir),
            ("mounts", &layout.mounts_dir),
            ("providers", &layout.providers_dir),
            ("credentials", &layout.credentials_file),
            ("config file", &layout.config_file),
        ] {
            section.push(Row::new(Glyph::Skip, key, WorkspaceLayout::display(path)));
        }
        section
    }

    async fn probe_docker_reachable(&self) -> (Option<Runtime>, ProbeResult) {
        use crate::runtime::DockerProbeOutcome;

        let target = match &self.docker_target {
            Ok(target) => target,
            Err(error) => return (None, ProbeResult::Err(error.clone())),
        };

        match Runtime::probe_docker(target).await {
            DockerProbeOutcome::Reachable(runtime) => (
                Some(runtime),
                ProbeResult::Ok("docker daemon responds".into()),
            ),
            DockerProbeOutcome::ConnectFailed(e) => {
                (None, ProbeResult::Err(format!("connect: {e}")))
            },
            DockerProbeOutcome::PingFailed(e) => (None, ProbeResult::Err(format!("ping: {e}"))),
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
            ProbeResult::Skipped(
                "macOS: native mount is NFS loopback; FUSE runs only inside the optional frontend container",
            )
        }
    }

    async fn probe_image_cached(&self, runtime: &Runtime, image: &ImageRef) -> ProbeResult {
        match runtime.inspect_image(image.as_str()).await {
            Ok(_) => ProbeResult::Ok(format!("{image} cached")),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) if names_registry(image.as_str()) => ProbeResult::Warn(format!(
                "{image} not cached (will pull on `omnifs frontend up`)"
            )),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => ProbeResult::Err(format!(
                "{image} not present locally; a dev image is never pulled, so `omnifs frontend up` \
                 cannot start (build it with `just frontend-image`)"
            )),
            Err(error) => ProbeResult::Err(format!("inspect: {error}")),
        }
    }

    fn probe_providers_discovered(&self) -> ProbeResult {
        match self.workspace.catalog().dir_status() {
            DirStatus::Present { wasm_count } if wasm_count > 0 => {
                match crate::commands::provider::provider_summaries(self.workspace.catalog()) {
                    Ok(summaries) => ProbeResult::Ok(format!(
                        "{} providers ({wasm_count} artifacts)",
                        summaries.len()
                    )),
                    Err(error) => ProbeResult::Err(format!("provider store unreadable: {error}")),
                }
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
        let credentials_file = &self.workspace.layout().credentials_file;
        let Some(parent) = credentials_file.parent() else {
            return ProbeResult::Err(format!(
                "credential file has no parent: {}",
                credentials_file.display()
            ));
        };
        if parent.exists() {
            ProbeResult::Ok(format!(
                "file {}",
                WorkspaceLayout::display(credentials_file)
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
        let config_file = &self.workspace.layout().config_file;
        if config_file.exists() {
            ProbeResult::Ok(WorkspaceLayout::display(config_file))
        } else {
            ProbeResult::Ok(format!(
                "(default; {} absent)",
                WorkspaceLayout::display(config_file)
            ))
        }
    }

    fn probe_mount_configs(&self) -> (ProbeResult, Vec<(String, ProbeResult)>) {
        let store = FileStore::new(&self.workspace.layout().credentials_file);
        let mounts = crate::mount_report::scan_user_mount_configs(
            self.workspace.catalog(),
            &self.mounts,
            &store,
        );
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
            // One-line detail keeps the message on the shared grid.
            let detail = invalid
                .iter()
                .map(|(name, error)| format!("{name}: {error}"))
                .collect::<Vec<_>>()
                .join("; ");
            ProbeResult::Err(format!(
                "{valid_count} valid, {} invalid: {detail}",
                invalid.len()
            ))
        };

        let auth_results = mounts
            .iter()
            .filter_map(|mount| {
                let UserMountStatus::Ready(mount) = mount else {
                    return None;
                };
                let result = ProbeResult::from_auth_summary(mount.auth.probe_summary());
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
            Some(provider) => format!("omnifs mount add {provider} --as {}", failure.mount),
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
                    && credential.health.needs_attention()
            })
            .map(|credential| LiveFinding {
                mount: mount.mount.clone(),
                state: "warn",
                message: format!(
                    "credential `{}` is {}",
                    credential.id,
                    credential_health_label(credential.health)
                ),
                fix: format!("omnifs mount reauth {}", mount.mount),
            })
            .peekable();
        // The aggregate mount health is derived from the same credentials, so
        // only fall back to it when no per-credential finding names the cause.
        if credential_findings.peek().is_some() {
            findings.extend(credential_findings);
        } else if let Some(health) = mount.auth_health
            && health.needs_attention()
        {
            findings.push(LiveFinding {
                mount: mount.mount.clone(),
                state: "warn",
                message: format!("credential health is {}", credential_health_label(health)),
                fix: format!("omnifs mount reauth {}", mount.mount),
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

#[cfg(test)]
mod golden {
    use super::*;
    use crate::ui::strip_ansi;

    fn probes() -> Vec<(&'static str, ProbeResult)> {
        vec![
            (
                "docker reachable",
                ProbeResult::Ok("docker daemon responds".to_string()),
            ),
            (
                "fuse",
                ProbeResult::Skipped("macOS: native mount is NFS loopback"),
            ),
            (
                "providers discovered",
                ProbeResult::Ok("9 providers (27 artifacts)".to_string()),
            ),
            (
                "credential store",
                ProbeResult::Warn("directory will be created on first write".to_string()),
            ),
            (
                "network",
                ProbeResult::Err("ghcr.io unreachable".to_string()),
            ),
        ]
    }

    fn live() -> LiveSection {
        LiveSection {
            skipped: None,
            findings: vec![LiveFinding {
                mount: "linear".to_string(),
                state: "warn",
                message: "credential `linear:oauth:default` is expired".to_string(),
                fix: "omnifs mount reauth linear".to_string(),
            }],
        }
    }

    #[test]
    fn doctor_grid() {
        let probes = probes();
        let mut diagnostics = Section::new("Diagnostics").counted(probes.len());
        for (name, result) in &probes {
            diagnostics.push(Row::new(
                result.glyph(),
                *name,
                result.message().to_string(),
            ));
        }
        let live = live();
        let mut report = Report::new();
        report.push(diagnostics);
        report.push(live_section(&live));
        // One err probe drives the failure verdict.
        report.push(Section::new(verdict_line(DoctorVerdict::Failures, 1, 1)));
        insta::assert_snapshot!(strip_ansi(&report.render()));
    }

    #[test]
    fn verdict_line_pluralizes() {
        assert_eq!(verdict_line(DoctorVerdict::Clean, 0, 0), "verdict: clean");
        assert_eq!(
            verdict_line(DoctorVerdict::Warnings, 1, 0),
            "verdict: 1 warning"
        );
        assert_eq!(
            verdict_line(DoctorVerdict::Failures, 0, 2),
            "verdict: 2 failures"
        );
    }

    #[test]
    fn live_failure_promotes_verdict_to_failure() {
        let live = LiveSection {
            skipped: None,
            findings: vec![LiveFinding {
                mount: "broken".to_string(),
                state: "err",
                message: "failed to load".to_string(),
                fix: "omnifs logs".to_string(),
            }],
        };
        let mut report = DoctorReport::new(OutputFormat::Text);
        report.record_live(&live);
        assert_eq!(report.verdict, DoctorVerdict::Failures);
    }

    #[test]
    fn doctor_json_preserves_schema_and_live_fixes() {
        let live = live();
        let payload = DoctorJson {
            verdict: DoctorVerdict::Warnings.label(),
            probes: vec![ProbeJson {
                name: "network".to_string(),
                state: "warn",
                message: "ghcr.io unreachable".to_string(),
            }],
            live,
        };
        insta::assert_snapshot!(serde_json::to_string_pretty(&payload).unwrap());
    }
}
