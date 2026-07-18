//! `omnifs doctor` — runtime + auth diagnostics, presented as a grouped
//! checklist (spec 3.9). The only remediation doctor can execute itself is a
//! mount reauth, spawned as a fresh `omnifs mount reauth <name>` subprocess
//! (mirroring how `daemon_launch.rs` spawns the daemon) rather than calling
//! into `commands::mount`'s internal API, so this module never couples to
//! that module's Rust shape.

use anyhow::Context as _;
use clap::Args;
use serde::Serialize;
use std::path::Path;

use omnifs_workspace::creds::CredentialStore;

use crate::docker::DockerClient;
use crate::docker::DockerTarget;
use crate::frontend_container::{frontend_container_name, resolve_frontend_image};
use crate::image::{ImageRef, names_registry};
use crate::inventory::{AuthState, DaemonState, Inventory, MountStatus, Severity};
use crate::ui::output::{Output, ResultVerdict};
use crate::ui::prompt::Confirm;
use crate::ui::render::{self, Capabilities, LedgerRow};
use crate::ui::style::{self, Glyph};
use omnifs_workspace::Workspace;

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
    pub async fn run(self, output: Output) -> anyhow::Result<DoctorVerdict> {
        let workspace = Workspace::resolve()?;
        let inventory = Inventory::collect(&workspace).await?;
        let docker_target = resolve_frontend_target(&workspace)
            .map_err(|error: anyhow::Error| format!("resolve target: {error:#}"));
        Doctor {
            workspace: &workspace,
            inventory,
            docker_target,
            output,
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
    let image = resolve_frontend_image(None, &config)?;
    let identity = workspace.identity();
    let container_name = frontend_container_name(identity.container_label())?;
    DockerTarget::new(
        container_name.as_str().to_string(),
        image.as_str().to_string(),
    )
}

struct Doctor<'a> {
    workspace: &'a Workspace,
    inventory: Inventory,
    /// The frontend's Docker target, or the error resolving it.
    docker_target: Result<DockerTarget, String>,
    output: Output,
}

/// Which group of spec 3.9's checklist a finding belongs to. A closed enum
/// rather than matching on the `check` string, so grouping cannot drift from
/// spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Environment,
    Workspace,
}

/// A remediation doctor knows how to execute itself. Today the only one is
/// re-running the exact `omnifs mount reauth <name>` command already
/// surfaced as a credentials finding's `fix` line.
#[derive(Debug, Clone)]
enum Remediation {
    MountReauth(String),
}

impl Remediation {
    fn command_line(&self) -> String {
        match self {
            Self::MountReauth(name) => format!("omnifs mount reauth {name}"),
        }
    }

    /// Spawn the fresh subprocess and require it to exit successfully. Array
    /// arguments only, never a shell string: the mount name came from the
    /// already-collected inventory, not from re-parsing the advisory `fix`
    /// text.
    fn apply(&self) -> anyhow::Result<()> {
        let binary = std::env::current_exe().context("resolve the omnifs executable")?;
        let status = match self {
            Self::MountReauth(name) => std::process::Command::new(&binary)
                .args(["mount", "reauth", name])
                .status()
                .with_context(|| format!("run `{}`", self.command_line()))?,
        };
        anyhow::ensure!(
            status.success(),
            "`{}` exited with {status}",
            self.command_line()
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
struct Finding {
    #[serde(skip)]
    section: Section,
    check: String,
    target: Option<String>,
    severity: Severity,
    message: String,
    fix: Option<String>,
    #[serde(skip)]
    remediation: Option<Remediation>,
}

#[derive(Debug, Clone, Serialize)]
struct DoctorResult {
    inventory: Inventory,
    findings: Vec<Finding>,
}

impl DoctorResult {
    fn verdict(&self) -> DoctorVerdict {
        let finding_severity = self
            .findings
            .iter()
            .map(|finding| finding.severity)
            .max()
            .unwrap_or(Severity::Positive);
        match (self.inventory.verdict(), finding_severity) {
            (_, Severity::Error) => DoctorVerdict::Failures,
            (crate::inventory::Verdict::Degraded, _) | (_, Severity::Attention) => {
                DoctorVerdict::Warnings
            },
            (crate::inventory::Verdict::Ok, Severity::Positive | Severity::Neutral) => {
                DoctorVerdict::Clean
            },
        }
    }
}

impl Finding {
    fn from_probe(
        section: Section,
        check: impl Into<String>,
        target: Option<String>,
        result: ProbeResult,
    ) -> Self {
        let (severity, message) = result.into_parts();
        Self {
            section,
            check: check.into(),
            target,
            severity,
            message,
            fix: None,
            remediation: None,
        }
    }

    /// One finding per mount whose credential needs attention (spec 3.9's
    /// worked `credentials` example), built entirely from data `Inventory`
    /// already collected: doctor invents no new auth check here.
    fn mount_auth(mount: &MountStatus) -> Option<Self> {
        let (message, command) = match &mount.auth {
            AuthState::Missing { command } => ("credential missing".to_owned(), command.clone()),
            AuthState::Expired { command } => ("token expired".to_owned(), command.clone()),
            AuthState::Error { message, command } => (message.clone(), command.clone()),
            AuthState::NotNeeded | AuthState::Ready => return None,
        };
        Some(Self {
            section: Section::Workspace,
            check: "credentials".to_owned(),
            target: Some(mount.name.clone()),
            severity: mount.auth.severity(),
            message,
            fix: Some(command),
            remediation: Some(Remediation::MountReauth(mount.name.clone())),
        })
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
    fn into_parts(self) -> (Severity, String) {
        match self {
            Self::Ok(message) => (Severity::Positive, message),
            Self::Warn(message) => (Severity::Attention, message),
            Self::Err(message) => (Severity::Error, message),
            Self::Skipped(message) => (Severity::Neutral, message.to_owned()),
        }
    }
}

/// One rendered checklist row: a finding or the synthesized daemon row,
/// stripped down to exactly what presentation needs.
struct Row {
    severity: Severity,
    key: String,
    value: String,
    fix: Option<String>,
}

impl Row {
    fn glyph(&self) -> Glyph {
        match self.severity {
            Severity::Positive => Glyph::Done,
            Severity::Neutral => Glyph::Skip,
            Severity::Attention => Glyph::Warn,
            Severity::Error => Glyph::Fail,
        }
    }

    fn ledger_row(&self) -> LedgerRow {
        LedgerRow::new(self.glyph(), self.key.clone(), self.value.clone())
    }
}

impl From<&Finding> for Row {
    fn from(finding: &Finding) -> Self {
        Self {
            severity: finding.severity,
            key: finding.check.clone(),
            value: finding.target.as_deref().map_or_else(
                || finding.message.clone(),
                |target| format!("{target} {}", finding.message),
            ),
            fix: finding.fix.clone(),
        }
    }
}

/// Split findings into the Environment/Workspace groups (spec 3.9); the
/// Daemon group's single row comes from `daemon_row`, not from `findings`.
fn build_rows(findings: &[Finding]) -> (Vec<Row>, Vec<Row>) {
    let mut environment = Vec::new();
    let mut workspace = Vec::new();
    for finding in findings {
        match finding.section {
            Section::Environment => environment.push(Row::from(finding)),
            Section::Workspace => workspace.push(Row::from(finding)),
        }
    }
    (environment, workspace)
}

fn daemon_row(inventory: &Inventory) -> Row {
    match inventory.daemon.state() {
        DaemonState::Running => Row {
            severity: Severity::Positive,
            key: "running".to_owned(),
            value: daemon_running_value(inventory),
            fix: None,
        },
        DaemonState::Starting => Row {
            severity: Severity::Attention,
            key: "starting".to_owned(),
            value: "daemon is still coming up".to_owned(),
            fix: None,
        },
        DaemonState::Degraded => Row {
            severity: Severity::Attention,
            key: "degraded".to_owned(),
            value: "daemon reports a degraded subsystem".to_owned(),
            fix: Some("omnifs status".to_owned()),
        },
        DaemonState::Stopped => Row {
            severity: Severity::Neutral,
            key: "stopped".to_owned(),
            value: "daemon is not running".to_owned(),
            fix: Some("omnifs up".to_owned()),
        },
        DaemonState::Failed => Row {
            severity: Severity::Error,
            key: "failed".to_owned(),
            value: "daemon is unhealthy".to_owned(),
            fix: Some("omnifs logs".to_owned()),
        },
        DaemonState::Unreachable => Row {
            severity: Severity::Error,
            key: "unreachable".to_owned(),
            value: "daemon record exists but the control socket did not answer".to_owned(),
            fix: Some("omnifs logs".to_owned()),
        },
    }
}

/// The running daemon's value cell (spec 3.9: `pid 31114, revision
/// 3f69473, up 2h`). Each part degrades independently: a fact `Inventory`
/// did not collect is omitted rather than faked.
fn daemon_running_value(inventory: &Inventory) -> String {
    let mut parts = Vec::new();
    if let Some(pid) = inventory.daemon.pid() {
        parts.push(format!("pid {pid}"));
    }
    if let Some(revision) = &inventory.mount_revision {
        parts.push(format!("revision {revision}"));
    }
    if let Some(uptime) = daemon_uptime(inventory) {
        parts.push(format!("up {uptime}"));
    }
    if parts.is_empty() {
        "running".to_owned()
    } else {
        parts.join(", ")
    }
}

fn daemon_uptime(inventory: &Inventory) -> Option<String> {
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    let record = inventory.daemon.runtime.as_ref()?;
    let started = OffsetDateTime::parse(&record.started_at, &Rfc3339).ok()?;
    let secs = (OffsetDateTime::now_utc() - started).whole_seconds();
    (secs >= 0).then(|| crate::docker::duration_words(secs))
}

/// Render one group: a bold heading, then each row indented two spaces
/// under it with its own `fix:` continuation line when it carries one
/// (spec 3.9). Key sizing is per group, matching the register's per-block
/// rule (2.1).
fn render_group(heading: &str, rows: &[Row], caps: Capabilities) -> String {
    let mut out = String::new();
    out.push_str(&render::heading(heading, caps));
    out.push('\n');
    let ledger_rows: Vec<LedgerRow> = rows.iter().map(Row::ledger_row).collect();
    let key_width = render::ledger_key_width(&ledger_rows);
    for (row, ledger_row) in rows.iter().zip(ledger_rows.iter()) {
        out.push_str("  ");
        out.push_str(&render::ledger_row_line(ledger_row, key_width, caps));
        out.push('\n');
        if let Some(fix) = &row.fix {
            let pad = 2 + render::ledger_value_column(key_width);
            out.push_str(&" ".repeat(pad));
            out.push_str("fix:  ");
            out.push_str(&style::accent(fix, caps.color));
            out.push('\n');
        }
    }
    out
}

/// The verdict line (spec 3.9): a plain "Everything checks out." when clean,
/// otherwise a failure/warning count plus the single actionable fix when
/// every problem row shares one.
fn verdict_line(rows: &[&Row], verdict: DoctorVerdict, caps: Capabilities) -> String {
    if verdict == DoctorVerdict::Clean {
        return render::sentence("Everything checks out.", caps);
    }
    let failures = rows
        .iter()
        .filter(|row| row.severity == Severity::Error)
        .count();
    let warnings = rows
        .iter()
        .filter(|row| row.severity == Severity::Attention)
        .count();
    let mut parts = Vec::new();
    if failures > 0 {
        parts.push(render::count(failures, "failure"));
    }
    if warnings > 0 {
        parts.push(render::count(warnings, "warning"));
    }
    let summary = if parts.is_empty() {
        // The daemon/inventory verdict is degraded for a reason this
        // group's rows don't individually carry (e.g. a frontend-only
        // degradation); name it honestly rather than claiming zero issues.
        "needs attention".to_owned()
    } else {
        parts.join(", ")
    };
    let fixable: Vec<&str> = rows
        .iter()
        .filter(|row| row.severity >= Severity::Attention)
        .filter_map(|row| row.fix.as_deref())
        .collect();
    match fixable[..] {
        [only] => format!("{summary}. Fix it:  {}", style::accent(only, caps.color)),
        _ => format!("{summary}."),
    }
}

/// Assemble the complete human checklist (spec 3.9): Environment, Workspace,
/// and Daemon groups, each separated by one blank line, then the verdict
/// line.
fn render_report(
    findings: &[Finding],
    inventory: &Inventory,
    verdict: DoctorVerdict,
    caps: Capabilities,
) -> String {
    let (environment, workspace) = build_rows(findings);
    let daemon = vec![daemon_row(inventory)];
    let mut out = String::new();
    out.push_str(&render_group("Environment", &environment, caps));
    out.push('\n');
    out.push_str(&render_group("Workspace", &workspace, caps));
    out.push('\n');
    out.push_str(&render_group("Daemon", &daemon, caps));
    out.push('\n');
    let all_rows: Vec<&Row> = environment
        .iter()
        .chain(workspace.iter())
        .chain(daemon.iter())
        .collect();
    out.push_str(&verdict_line(&all_rows, verdict, caps));
    out.push('\n');
    out
}

/// The remediations doctor is willing to offer to run, or `None` when the
/// warnings/failures on this run are not all fixable through a known,
/// doctor-owned remediation (spec 3.9: "when every warning has a safe known
/// remediation").
fn remediable_fixes(findings: &[Finding]) -> Option<Vec<&Remediation>> {
    let actionable: Vec<&Finding> = findings
        .iter()
        .filter(|finding| finding.severity >= Severity::Attention)
        .collect();
    if actionable.is_empty() {
        return None;
    }
    let remediations: Vec<&Remediation> = actionable
        .iter()
        .filter_map(|finding| finding.remediation.as_ref())
        .collect();
    (remediations.len() == actionable.len()).then_some(remediations)
}

/// Offer to apply every remediable fix (spec 3.9). `--no-input`, structured
/// modes, and a non-interactive session all degrade to report-only, matching
/// the rest of the CLI's prompt policy rather than inventing a doctor-local
/// rule.
fn offer_fix(output: &Output, findings: &[Finding]) -> anyhow::Result<()> {
    let Some(remediations) = remediable_fixes(findings) else {
        return Ok(());
    };
    let apply = if output.yes() {
        true
    } else if output.ensure_prompt_allowed().is_err() || !crate::ui::prompt::is_terminal() {
        false
    } else {
        let question = if remediations.len() == 1 {
            "Apply 1 fix now?".to_owned()
        } else {
            format!("Apply {} fixes now?", remediations.len())
        };
        Confirm::new(question).ask_with_output(output)?
    };
    if !apply {
        return Ok(());
    }
    let caps = render::stdout_capabilities();
    let ledger_rows: Vec<LedgerRow> = remediations
        .iter()
        .map(|remediation| LedgerRow::new(Glyph::Done, "fix", remediation.command_line()))
        .collect();
    let key_width = render::ledger_key_width(&ledger_rows);
    for (remediation, mut ledger_row) in remediations.into_iter().zip(ledger_rows) {
        let outcome = remediation.apply();
        ledger_row.glyph = if outcome.is_ok() {
            Glyph::Done
        } else {
            Glyph::Fail
        };
        crate::ui::print_raw(&format!(
            "{}\n",
            render::ledger_row_line(&ledger_row, key_width, caps)
        ));
        outcome?;
    }
    Ok(())
}

impl Doctor<'_> {
    async fn run(self) -> anyhow::Result<DoctorVerdict> {
        let mut findings = Vec::new();

        let (runtime, docker_result) = self.probe_docker_reachable().await;
        let docker_ok = matches!(docker_result, ProbeResult::Ok(_));
        findings.push(Finding::from_probe(
            Section::Environment,
            "docker",
            None,
            docker_result,
        ));

        findings.push(Finding::from_probe(
            Section::Environment,
            "fuse",
            None,
            self.probe_fuse(),
        ));

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
        findings.push(Finding::from_probe(
            Section::Environment,
            "image",
            None,
            image_result,
        ));

        findings.push(Finding::from_probe(
            Section::Workspace,
            "credential store",
            None,
            self.probe_credential_store(),
        ));
        findings.push(Finding::from_probe(
            Section::Workspace,
            "ssh-agent",
            None,
            self.probe_ssh_agent(),
        ));
        findings.push(Finding::from_probe(
            Section::Workspace,
            "config",
            None,
            self.probe_config_file(),
        ));

        findings.push(Finding::from_probe(
            Section::Environment,
            "network",
            None,
            self.probe_network().await,
        ));

        findings.extend(self.inventory.mounts.iter().filter_map(Finding::mount_auth));

        let result = DoctorResult {
            inventory: self.inventory,
            findings,
        };
        let verdict = result.verdict();
        if self.output.is_structured() {
            self.output.emit_result(
                match verdict {
                    DoctorVerdict::Clean => ResultVerdict::Ok,
                    DoctorVerdict::Warnings | DoctorVerdict::Failures => ResultVerdict::Degraded,
                },
                result,
            )?;
        } else {
            let caps = render::stdout_capabilities();
            crate::ui::print_raw(&render_report(
                &result.findings,
                &result.inventory,
                verdict,
                caps,
            ));
            offer_fix(&self.output, &result.findings)?;
        }
        Ok(verdict)
    }

    async fn probe_docker_reachable(&self) -> (Option<DockerClient>, ProbeResult) {
        use crate::docker::DockerProbeOutcome;

        let target = match &self.docker_target {
            Ok(target) => target,
            Err(error) => return (None, ProbeResult::Err(error.clone())),
        };

        match DockerClient::probe_docker(target).await {
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

    async fn probe_image_cached(&self, runtime: &DockerClient, image: &ImageRef) -> ProbeResult {
        match runtime.inspect_image(image.as_str()).await {
            Ok(_) => ProbeResult::Ok(format!("{image} cached")),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) if names_registry(image.as_str()) => ProbeResult::Warn(format!(
                "{image} not cached (will pull on `omnifs frontend enable`)"
            )),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => ProbeResult::Err(format!(
                "{image} not present locally; a dev image is never pulled, so `omnifs frontend enable` \
                 cannot start (build it with `just frontend-image`)"
            )),
            Err(error) => ProbeResult::Err(format!("inspect: {error}")),
        }
    }

    fn probe_credential_store(&self) -> ProbeResult {
        let diagnostic = self.workspace.credentials().diagnostic();
        if !diagnostic.parent_exists {
            ProbeResult::Warn(format!(
                "credential directory will be created on first write: {}",
                diagnostic.display
            ))
        } else if !diagnostic.exists {
            ProbeResult::Warn(format!(
                "credential store not created yet: {}",
                diagnostic.display
            ))
        } else {
            match self.workspace.credentials().list() {
                Ok(Some(keys)) => ProbeResult::Ok(format!(
                    "{} in {}",
                    crate::ui::render::count(keys.len(), "credential"),
                    diagnostic.display
                )),
                Ok(None) => ProbeResult::Ok(format!(
                    "credential store available at {}",
                    diagnostic.display
                )),
                Err(error) => ProbeResult::Err(format!(
                    "credential store {} unreadable: {error}",
                    diagnostic.display
                )),
            }
        }
    }

    #[allow(clippy::unused_self)] // Kept as a Doctor probe method for a uniform probe surface.
    fn probe_ssh_agent(&self) -> ProbeResult {
        match std::env::var_os("SSH_AUTH_SOCK") {
            Some(sock) if Path::new(&sock).exists() => {
                ProbeResult::Ok(omnifs_workspace::display(Path::new(&sock)))
            },
            Some(_) => ProbeResult::Warn("SSH_AUTH_SOCK set but socket not found".into()),
            None => ProbeResult::Warn("SSH_AUTH_SOCK unset; git callouts will fail".into()),
        }
    }

    fn probe_config_file(&self) -> ProbeResult {
        let diagnostic = self.workspace.config_diagnostic();
        if diagnostic.exists {
            ProbeResult::Ok(diagnostic.display)
        } else {
            ProbeResult::Ok(format!("defaults ({} absent)", diagnostic.display))
        }
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
}

#[cfg(test)]
mod golden {
    use super::*;
    use crate::test_support::fixture_workspace;
    use tempfile::TempDir;

    fn probes() -> Vec<Finding> {
        vec![
            Finding::from_probe(
                Section::Environment,
                "docker",
                None,
                ProbeResult::Ok("docker daemon responds".to_string()),
            ),
            Finding::from_probe(
                Section::Environment,
                "fuse",
                None,
                ProbeResult::Skipped("macOS: native mount is NFS loopback"),
            ),
            Finding::from_probe(
                Section::Environment,
                "network",
                None,
                ProbeResult::Ok("ghcr.io reachable".to_string()),
            ),
            Finding::from_probe(
                Section::Workspace,
                "config",
                None,
                ProbeResult::Ok("defaults (~/.omnifs/config.toml absent)".to_string()),
            ),
        ]
    }

    fn targeted_finding() -> Finding {
        Finding {
            section: Section::Workspace,
            check: "credentials".to_string(),
            target: Some("github".to_string()),
            severity: Severity::Attention,
            message: "token expired".to_string(),
            fix: Some("omnifs mount reauth github".to_string()),
            remediation: Some(Remediation::MountReauth("github".to_string())),
        }
    }

    fn caps(color: bool) -> Capabilities {
        Capabilities {
            width: 120,
            is_tty: color,
            color,
            quiet: false,
        }
    }

    fn running_inventory() -> Inventory {
        Inventory::test(DaemonState::Running, Vec::new(), Vec::new())
    }

    #[test]
    fn healthy_checklist_ends_with_everything_checks_out() {
        let findings = probes();
        let inventory = running_inventory();
        let rendered = render_report(&findings, &inventory, DoctorVerdict::Clean, caps(false));
        assert!(rendered.contains("Environment"), "{rendered}");
        assert!(rendered.contains("Workspace"), "{rendered}");
        assert!(rendered.contains("Daemon"), "{rendered}");
        assert!(
            rendered.trim_end().ends_with("Everything checks out."),
            "{rendered}"
        );
        // Groups are separated by a blank line (spec 2.1's block rule), not
        // run together.
        assert!(rendered.contains("\n\nWorkspace\n"), "{rendered}");
        assert!(rendered.contains("\n\nDaemon\n"), "{rendered}");
    }

    #[test]
    fn grouped_checklist_matches_the_documented_shape_with_a_warning_row() {
        let mut findings = probes();
        findings.push(targeted_finding());
        let inventory = running_inventory();
        let rendered = render_report(&findings, &inventory, DoctorVerdict::Warnings, caps(false));
        let lines: Vec<&str> = rendered.lines().collect();

        let credentials_index = lines
            .iter()
            .position(|line| line.trim_start().starts_with("! credentials"))
            .expect("credentials warning row");
        assert!(
            lines[credentials_index].contains("github token expired"),
            "{rendered}"
        );
        assert_eq!(
            lines[credentials_index + 1].trim(),
            "fix:  omnifs mount reauth github",
            "{rendered}"
        );

        assert!(rendered.contains("  ✓ docker"), "{rendered}");
        assert!(rendered.contains("  • fuse"), "{rendered}");
        assert!(rendered.contains("  ✓ running"), "{rendered}");

        let verdict = lines.last().copied().unwrap_or_default();
        assert_eq!(verdict, "1 warning. Fix it:  omnifs mount reauth github");
    }

    #[test]
    fn verdict_combines_inventory_with_maximum_finding_severity() {
        let clean = DoctorResult {
            inventory: Inventory::test(
                crate::inventory::DaemonState::Stopped,
                Vec::new(),
                Vec::new(),
            ),
            findings: Vec::new(),
        };
        assert_eq!(clean.verdict(), DoctorVerdict::Clean);

        let degraded = DoctorResult {
            inventory: Inventory::test(
                crate::inventory::DaemonState::Failed,
                Vec::new(),
                Vec::new(),
            ),
            findings: Vec::new(),
        };
        assert_eq!(degraded.verdict(), DoctorVerdict::Warnings);

        let mut warnings = clean.clone();
        warnings.findings.push(targeted_finding());
        assert_eq!(warnings.verdict(), DoctorVerdict::Warnings);

        let mut failures = warnings;
        failures.findings.push(Finding {
            section: Section::Environment,
            check: "broken".to_owned(),
            target: None,
            severity: Severity::Error,
            message: "failed to load".to_owned(),
            fix: Some("omnifs logs".to_owned()),
            remediation: None,
        });
        assert_eq!(failures.verdict(), DoctorVerdict::Failures);
    }

    #[test]
    fn doctor_json_preserves_inventory_and_findings_and_skips_presentation_only_fields() {
        let payload = DoctorResult {
            inventory: Inventory::test(
                crate::inventory::DaemonState::Stopped,
                Vec::new(),
                Vec::new(),
            ),
            findings: vec![targeted_finding()],
        };
        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string_pretty(&payload).unwrap()).unwrap();
        assert!(value.get("inventory").is_some());
        assert_eq!(value["findings"][0]["check"], "credentials");
        assert_eq!(value["findings"][0]["target"], "github");
        assert_eq!(value["findings"][0]["severity"], "attention");
        assert_eq!(value["findings"][0]["fix"], "omnifs mount reauth github");
        // `section` and `remediation` are presentation/execution-only and
        // must not grow the machine contract.
        assert!(value["findings"][0].get("section").is_none());
        assert!(value["findings"][0].get("remediation").is_none());
    }

    #[test]
    fn remediable_fixes_requires_every_warning_to_carry_a_known_remediation() {
        let all_remediable = vec![targeted_finding()];
        assert!(remediable_fixes(&all_remediable).is_some());

        let mixed = vec![
            targeted_finding(),
            Finding {
                section: Section::Environment,
                check: "network".to_owned(),
                target: None,
                severity: Severity::Attention,
                message: "ghcr.io unreachable".to_owned(),
                fix: None,
                remediation: None,
            },
        ];
        assert!(remediable_fixes(&mixed).is_none());

        assert!(remediable_fixes(&probes()).is_none());
    }

    fn probe_credential_result(root: &std::path::Path) -> ProbeResult {
        let workspace = fixture_workspace(root);
        let doctor = Doctor {
            workspace: &workspace,
            inventory: Inventory::test(
                crate::inventory::DaemonState::Stopped,
                Vec::new(),
                Vec::new(),
            ),
            docker_target: Err("test".to_owned()),
            output: Output::new(crate::ui::output::OutputMode::Human, false),
        };
        doctor.probe_credential_store()
    }

    #[test]
    fn credential_store_probe_distinguishes_missing_valid_and_invalid_files() {
        let missing = TempDir::new().unwrap();
        let result = probe_credential_result(missing.path());
        assert!(
            matches!(result, ProbeResult::Warn(message) if message.contains("not created yet"))
        );

        let valid = TempDir::new().unwrap();
        let valid_path = valid.path().join("credentials.json");
        std::fs::create_dir_all(valid.path()).unwrap();
        std::fs::write(&valid_path, r#"{"version":1,"entries":{}}"#).unwrap();
        let result = probe_credential_result(valid.path());
        assert!(
            matches!(result, ProbeResult::Ok(message) if message.contains("0 credentials") && message.contains("credentials.json"))
        );

        let invalid = TempDir::new().unwrap();
        let invalid_path = invalid.path().join("credentials.json");
        std::fs::write(&invalid_path, "not json").unwrap();
        let result = probe_credential_result(invalid.path());
        assert!(
            matches!(result, ProbeResult::Err(message) if message.contains("credentials.json") && message.contains("unreadable"))
        );

        let unsupported = TempDir::new().unwrap();
        let unsupported_path = unsupported.path().join("credentials.json");
        std::fs::write(&unsupported_path, r#"{"version":99,"entries":{}}"#).unwrap();
        let result = probe_credential_result(unsupported.path());
        assert!(
            matches!(result, ProbeResult::Err(message) if message.contains("version") && message.contains("99"))
        );

        let bad_key = TempDir::new().unwrap();
        let bad_key_path = bad_key.path().join("credentials.json");
        std::fs::write(
            &bad_key_path,
            r#"{"version":1,"entries":{"not-a-credential-key":{"kind":"static-token","access_token":"x","refresh_token":null,"expires_at":null,"token_type":"Bearer","stored_at":"1970-01-01T00:00:00Z","last_validated":null,"scopes":[],"upstream_identity":null,"extras":{}}}}"#,
        )
        .unwrap();
        let result = probe_credential_result(bad_key.path());
        assert!(
            matches!(result, ProbeResult::Err(message) if message.contains("credential") && message.contains("key"))
        );
    }
}
