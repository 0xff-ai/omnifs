//! `omnifs doctor` — daemon-owned diagnostics, presented as a grouped checklist.
//! Mount reauth remains a client-side subprocess because it needs interactive
//! credentials; all other remediations run through the daemon control plane.

use anyhow::Context as _;
use omnifs_api::{
    DoctorCheckKind, DoctorExecutor, DoctorFinding, DoctorRepairOutcome, DoctorRepairState,
    DoctorSection, DoctorSeverity, RunDoctorReport,
};
use omnifs_bootstrap::Profile;
use serde::Serialize;
use std::collections::BTreeMap;

use crate::commands::daemon_start;
use crate::inventory::{DaemonFacts, DaemonHealth, DaemonProbe, Inventory, Severity};
use crate::rpc::RpcClient;
use crate::ui::output::{Output, ResultVerdict};
use crate::ui::prompt::Confirm;
use crate::ui::render::{self, Capabilities, LedgerRow};
use crate::ui::style::{self, Glyph};

/// Aggregate result of a completed diagnostic run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DoctorVerdict {
    Clean,
    Warnings,
    Failures,
}

pub async fn run(output: Output) -> anyhow::Result<DoctorVerdict> {
    match daemon_start::start(&output).await {
        Ok(()) => run_via_daemon(output).await,
        Err(error) => run_degraded(error, output).await,
    }
}

/// Which group of the checklist a finding belongs to. A closed enum
/// rather than matching on the `check` string, so grouping cannot drift from
/// spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Environment,
    Profile,
    Mounts,
    Filesystems,
}

/// Which specific check a finding reports. A closed enum sitting next to
/// [`Section`] instead of a bare string, so a check's identity cannot drift
/// from its own spelling the way five repeated string literals could. Each
/// variant's wire and display text is the exact string doctor has always
/// emitted for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
enum Check {
    #[serde(rename = "docker")]
    Docker,
    #[serde(rename = "fuse")]
    Fuse,
    #[serde(rename = "image")]
    Image,
    #[serde(rename = "credential store")]
    CredentialStore,
    #[serde(rename = "ssh-agent")]
    SshAgent,
    #[serde(rename = "config")]
    Config,
    #[serde(rename = "network")]
    Network,
    #[serde(rename = "daemon identity")]
    DaemonIdentity,
    #[serde(rename = "credentials")]
    Credentials,
    #[serde(rename = "filesystem state")]
    FilesystemState,
    #[serde(rename = "stray filesystem")]
    StrayFilesystem,
    #[serde(rename = "stale filesystem state")]
    StaleFilesystemState,
    #[serde(rename = "docker filesystem ownership")]
    DockerFilesystemOwnership,
    #[serde(rename = "libkrun filesystem ownership")]
    LibkrunFilesystemOwnership,
}

impl Check {
    const fn label(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::Fuse => "fuse",
            Self::Image => "image",
            Self::CredentialStore => "credential store",
            Self::SshAgent => "ssh-agent",
            Self::Config => "config",
            Self::Network => "network",
            Self::DaemonIdentity => "daemon identity",
            Self::Credentials => "credentials",
            Self::FilesystemState => "filesystem state",
            Self::StrayFilesystem => "stray filesystem",
            Self::StaleFilesystemState => "stale filesystem state",
            Self::DockerFilesystemOwnership => "docker filesystem ownership",
            Self::LibkrunFilesystemOwnership => "libkrun filesystem ownership",
        }
    }
}

/// A remediation doctor knows how to execute itself.
#[derive(Debug, Clone)]
enum Remediation {
    MountReauth(String),
    DaemonExecuted {
        id: String,
        command_line: String,
    },
    CleanStaleInstance {
        identity: omnifs_bootstrap::DaemonIdentity,
    },
}

impl Remediation {
    fn command_line(&self) -> String {
        match self {
            Self::MountReauth(name) => format!("omnifs mount reauth {name}"),
            Self::DaemonExecuted { command_line, .. } => command_line.clone(),
            Self::CleanStaleInstance { .. } => {
                "omnifs doctor (clean stale daemon identity)".to_owned()
            },
        }
    }

    /// Spawn the fresh subprocess and require it to exit successfully. Array
    /// arguments only, never a shell string: the mount name came from the
    /// already-collected inventory, not from re-parsing the advisory `fix`
    /// text.
    fn apply(&self, profile: &Profile) -> anyhow::Result<()> {
        match self {
            // Only this variant needs the CLI's own path, so it resolves it
            // itself instead of every variant paying for a lookup it never uses.
            Self::MountReauth(name) => {
                let binary = std::env::current_exe().context("resolve the omnifs executable")?;
                std::process::Command::new(&binary)
                    .args(["mount", "reauth", name])
                    .status()
                    .with_context(|| format!("run `{}`", self.command_line()))
                    .and_then(|status| {
                        anyhow::ensure!(
                            status.success(),
                            "`{}` exited with {status}",
                            self.command_line()
                        );
                        Ok(())
                    })
            },
            Self::DaemonExecuted { .. } => {
                anyhow::bail!("daemon remediation must run through the control plane")
            },
            Self::CleanStaleInstance { identity } => {
                if profile.remove_daemon_bootstrap_if(identity)? {
                    return Ok(());
                }
                match profile.read_process_identity()? {
                    None => Ok(()),
                    Some(current) if current == *identity => {
                        anyhow::bail!("stale daemon identity still exists after cleanup")
                    },
                    Some(_) => {
                        anyhow::bail!("daemon identity changed; refusing to remove its replacement")
                    },
                }
            },
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct Finding {
    #[serde(skip)]
    section: Section,
    check: Check,
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
    /// Repairs attempted this run. Empty when nothing was remediable, the
    /// operator declined, or (structured mode only) nothing was authorized
    /// via `--yes`. Human mode renders these as they complete instead of
    /// carrying them here; this field exists for the JSON/JSONL contract.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    repairs: Vec<Repair>,
}

/// Outcome vocabulary shared with [`crate::ui::consent::OutcomeState`]:
/// applied (done), failed, or skipped (never attempted). `MountReauth` is
/// always skipped in structured mode rather than spawned, since a machine
/// caller runs the fix command itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum RepairState {
    Applied,
    Failed,
    Skipped,
}

/// One remediation's outcome, independent of rendering.
#[derive(Debug, Clone, Serialize)]
struct Repair {
    command_line: String,
    state: RepairState,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl Repair {
    fn applied(command_line: String) -> Self {
        Self {
            command_line,
            state: RepairState::Applied,
            error: None,
        }
    }

    fn failed(command_line: String, error: String) -> Self {
        Self {
            command_line,
            state: RepairState::Failed,
            error: Some(error),
        }
    }

    fn skipped(command_line: String) -> Self {
        Self {
            command_line,
            state: RepairState::Skipped,
            error: None,
        }
    }
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
            (_, Severity::Failure) => DoctorVerdict::Failures,
            (ResultVerdict::Degraded, _) | (_, Severity::Attention) => DoctorVerdict::Warnings,
            (ResultVerdict::Ok, Severity::Positive | Severity::Neutral) => DoctorVerdict::Clean,
        }
    }
}

impl From<DoctorVerdict> for ResultVerdict {
    fn from(verdict: DoctorVerdict) -> Self {
        match verdict {
            DoctorVerdict::Clean => Self::Ok,
            DoctorVerdict::Warnings | DoctorVerdict::Failures => Self::Degraded,
        }
    }
}

impl Finding {
    fn from_api(finding: DoctorFinding, remediations: &BTreeMap<String, Remediation>) -> Self {
        let DoctorFinding {
            section,
            check,
            target,
            severity,
            message,
            fix,
            remediation_id,
        } = finding;
        Self {
            section: section.into(),
            check: check.into(),
            target,
            severity: severity.into(),
            message,
            fix,
            remediation: remediation_id.and_then(|id| remediations.get(&id).cloned()),
        }
    }
}

impl From<DoctorSection> for Section {
    fn from(section: DoctorSection) -> Self {
        match section {
            DoctorSection::Environment => Self::Environment,
            DoctorSection::Profile => Self::Profile,
            DoctorSection::Mounts => Self::Mounts,
            DoctorSection::Filesystems => Self::Filesystems,
        }
    }
}

impl From<DoctorCheckKind> for Check {
    fn from(check: DoctorCheckKind) -> Self {
        match check {
            DoctorCheckKind::Docker => Self::Docker,
            DoctorCheckKind::Fuse => Self::Fuse,
            DoctorCheckKind::Image => Self::Image,
            DoctorCheckKind::Network => Self::Network,
            DoctorCheckKind::SshAgent => Self::SshAgent,
            DoctorCheckKind::Config => Self::Config,
            DoctorCheckKind::CredentialStore => Self::CredentialStore,
            DoctorCheckKind::Credentials => Self::Credentials,
            DoctorCheckKind::FilesystemState => Self::FilesystemState,
            DoctorCheckKind::StrayFilesystem => Self::StrayFilesystem,
            DoctorCheckKind::StaleFilesystemState => Self::StaleFilesystemState,
            DoctorCheckKind::DockerFilesystemOwnership => Self::DockerFilesystemOwnership,
            DoctorCheckKind::LibkrunFilesystemOwnership => Self::LibkrunFilesystemOwnership,
        }
    }
}

impl From<DoctorSeverity> for Severity {
    fn from(severity: DoctorSeverity) -> Self {
        match severity {
            DoctorSeverity::Positive => Self::Positive,
            DoctorSeverity::Neutral => Self::Neutral,
            DoctorSeverity::Attention => Self::Attention,
            DoctorSeverity::Failure => Self::Failure,
        }
    }
}

fn remediations_from_report(report: &RunDoctorReport) -> BTreeMap<String, Remediation> {
    report
        .remediations
        .iter()
        .map(|remediation| {
            let local = match &remediation.executor {
                DoctorExecutor::Daemon => Remediation::DaemonExecuted {
                    id: remediation.id.clone(),
                    command_line: remediation.command_line.clone(),
                },
                DoctorExecutor::ClientMountReauth { mount } => {
                    Remediation::MountReauth(mount.clone())
                },
            };
            (remediation.id.clone(), local)
        })
        .collect()
}

fn findings_from_report(report: &RunDoctorReport) -> Vec<Finding> {
    let remediations = remediations_from_report(report);
    report
        .findings
        .iter()
        .cloned()
        .map(|finding| Finding::from_api(finding, &remediations))
        .collect()
}

fn stale_process_identity_finding(probe: &DaemonProbe, endpoint: &Profile) -> Option<Finding> {
    if !matches!(probe, DaemonProbe::Unreachable { .. }) {
        return None;
    }
    let identity = endpoint.read_process_identity().ok()??;
    if identity.still_identifies_running_process() {
        return None;
    }
    let remediation = Remediation::CleanStaleInstance { identity };
    Some(Finding {
        section: Section::Profile,
        check: Check::DaemonIdentity,
        target: None,
        severity: Severity::Attention,
        message: "recorded daemon process no longer exists".to_owned(),
        fix: Some(remediation.command_line()),
        remediation: Some(remediation),
    })
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
            Severity::Failure => Glyph::Fail,
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
            key: finding.check.label().to_owned(),
            value: finding.target.as_deref().map_or_else(
                || finding.message.clone(),
                |target| format!("{target} {}", finding.message),
            ),
            fix: finding.fix.clone(),
        }
    }
}

/// Split findings into the Environment/Profile groups; the
/// Daemon group's single row comes from `daemon_row`, not from `findings`.
fn build_rows(findings: &[Finding]) -> (Vec<Row>, Vec<Row>, Vec<Row>, Vec<Row>) {
    let mut environment = Vec::new();
    let mut profile = Vec::new();
    let mut mounts = Vec::new();
    let mut filesystems = Vec::new();
    for finding in findings {
        match finding.section {
            Section::Environment => environment.push(Row::from(finding)),
            Section::Profile => profile.push(Row::from(finding)),
            Section::Mounts => mounts.push(Row::from(finding)),
            Section::Filesystems => filesystems.push(Row::from(finding)),
        }
    }
    (environment, profile, mounts, filesystems)
}

/// Severity and key come from `DaemonHealth::descriptor`, the one mapping
/// shared with `omnifs status`'s context strip; `value` and `fix` are
/// doctor's own remediation-facing prose, which the shared descriptor
/// (severity and a bare label only) never carried.
fn daemon_row(inventory: &Inventory) -> Row {
    let (severity, key) = inventory.daemon.health().descriptor();
    let (value, fix): (String, Option<String>) = match inventory.daemon.health() {
        DaemonHealth::Running => (daemon_running_value(inventory), None),
        DaemonHealth::Starting => ("daemon is still coming up".to_owned(), None),
        DaemonHealth::Degraded => (
            "daemon reports a degraded subsystem".to_owned(),
            Some("omnifs status".to_owned()),
        ),
        DaemonHealth::Stopped => ("daemon is not running".to_owned(), None),
        DaemonHealth::Failed => (
            "daemon is unhealthy".to_owned(),
            Some("omnifs logs".to_owned()),
        ),
        DaemonHealth::Unreachable => (
            "daemon identity exists but the control socket did not answer".to_owned(),
            Some("omnifs logs".to_owned()),
        ),
    };
    Row {
        severity,
        key: key.to_owned(),
        value,
        fix,
    }
}

/// The running daemon's value cell. Each part degrades independently: a fact `Inventory`
/// did not collect is omitted rather than faked.
fn daemon_running_value(inventory: &Inventory) -> String {
    let mut parts = Vec::new();
    if let Some(pid) = inventory.daemon.pid() {
        parts.push(format!("pid {pid}"));
    }
    if let Some(revision) = &inventory.durable_revision {
        parts.push(format!("revision {}", revision.get()));
    }
    if parts.is_empty() {
        "running".to_owned()
    } else {
        parts.join(", ")
    }
}

/// Render one group: a bold heading, then each row indented two spaces
/// Key sizing is per group, matching the register's per-block
/// rule (2.1).
fn render_group(heading: &str, rows: &[Row], caps: Capabilities) -> String {
    let mut out = String::new();
    out.push_str(&render::heading(heading, caps));
    out.push('\n');
    let ledger_rows: Vec<LedgerRow> = rows.iter().map(Row::ledger_row).collect();
    let key_width = render::ledger_key_width(&ledger_rows);
    for ledger_row in &ledger_rows {
        out.push_str("  ");
        out.push_str(&render::ledger_row_line(ledger_row, key_width, caps));
        out.push('\n');
    }
    out
}

/// The verdict line: a plain "Everything checks out." when clean,
/// otherwise a failure/warning count plus the single actionable fix when
/// every problem row shares one.
fn verdict_line(rows: &[&Row], verdict: DoctorVerdict, caps: Capabilities) -> String {
    if verdict == DoctorVerdict::Clean {
        return render::sentence("Everything checks out.", caps);
    }
    let failures = rows
        .iter()
        .filter(|row| row.severity == Severity::Failure)
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
        // group's rows don't individually carry (e.g. a filesystem-only
        // degradation); name it honestly rather than claiming zero issues.
        "needs attention".to_owned()
    } else {
        parts.join(", ")
    };
    let mut problems = rows
        .iter()
        .filter(|row| row.severity >= Severity::Attention);
    let shared_fix = problems.next().and_then(|row| row.fix.as_deref());
    let shared_fix =
        shared_fix.filter(|action| problems.all(|row| row.fix.as_deref() == Some(*action)));
    match shared_fix {
        Some(action) => format!("{summary}. Fix it:  {}", style::accent(action, caps.color)),
        None => format!("{summary}."),
    }
}

/// Assemble the complete human checklist: Environment, Profile,
/// and Daemon groups, each separated by one blank line, then the verdict
/// line.
fn render_report(
    findings: &[Finding],
    inventory: &Inventory,
    verdict: DoctorVerdict,
    caps: Capabilities,
) -> String {
    let (environment, profile, mounts, filesystems) = build_rows(findings);
    let daemon = vec![daemon_row(inventory)];
    let mut out = String::new();
    for (name, rows) in [
        ("Environment", &environment),
        ("Profile", &profile),
        ("Mounts", &mounts),
        ("Filesystems", &filesystems),
        ("Daemon", &daemon),
    ] {
        out.push_str(&render_group(name, rows, caps));
        out.push('\n');
    }
    let all_rows: Vec<&Row> = environment
        .iter()
        .chain(profile.iter())
        .chain(mounts.iter())
        .chain(filesystems.iter())
        .chain(daemon.iter())
        .collect();
    out.push_str(&verdict_line(&all_rows, verdict, caps));
    out.push('\n');
    out
}

/// The remediations doctor is willing to offer to run, or `None` when the
/// warnings/failures on this run are not all fixable through a known,
/// doctor-owned remediation.
fn remediable_fixes(findings: &[Finding]) -> Vec<Remediation> {
    let mut seen = std::collections::BTreeSet::new();
    findings
        .iter()
        .filter_map(|finding| finding.remediation.as_ref())
        .filter(|remediation| seen.insert(remediation.command_line()))
        .cloned()
        .collect()
}

#[derive(Debug, Default)]
struct RepairSummary {
    attempted: usize,
    failed: usize,
}

/// Show the complete repair set, ask once, then continue through independent
/// failures. Fresh ownership checks still run inside each remediation.
/// Structured mode has no terminal to confirm on, so `--yes` is the only way
/// a machine caller authorizes repair, and it never prints: every outcome
/// comes back in `repairs` for the caller to fold into the JSON result
/// instead. `MountReauth` is a genuine interactive sign-in, so structured
/// mode always leaves it for the caller to run itself rather than spawning
/// it with inherited stdio nobody can answer.
async fn diagnose_via_daemon(rpc: &RpcClient) -> anyhow::Result<DoctorResult> {
    let inventory = Inventory::collect_rpc().await?;
    let report = rpc.run_doctor().await?;
    Ok(DoctorResult {
        inventory,
        findings: findings_from_report(&report),
        repairs: Vec::new(),
    })
}

async fn run_via_daemon(output: Output) -> anyhow::Result<DoctorVerdict> {
    let profile = Profile::resolve()?;
    let rpc = RpcClient::resolve()?;
    let mut result = diagnose_via_daemon(&rpc).await?;
    let mut verdict = result.verdict();
    let structured = output.is_structured();

    if !structured {
        let caps = render::stdout_capabilities();
        output.report(render_report(
            &result.findings,
            &result.inventory,
            verdict,
            caps,
        ));
    }

    let (summary, repairs) = offer_fix(Some(&rpc), &profile, &output, &result.findings).await?;
    if summary.attempted > 0 {
        result = diagnose_via_daemon(&rpc).await?;
        verdict = result.verdict();
    }

    if structured {
        result.repairs = repairs;
        output.emit_result(ResultVerdict::from(verdict), result)?;
    } else if summary.attempted > 0 {
        output.narrate("");
        output.narrate(format!(
            "Repairs complete: {} attempted, {} failed. Final state: {}.",
            summary.attempted,
            summary.failed,
            doctor_verdict_label(verdict)
        ));
    }
    Ok(verdict)
}

async fn run_degraded(error: anyhow::Error, output: Output) -> anyhow::Result<DoctorVerdict> {
    let profile = Profile::resolve()?;
    run_degraded_at(error, &profile, output).await
}

async fn run_degraded_at(
    error: anyhow::Error,
    profile: &Profile,
    output: Output,
) -> anyhow::Result<DoctorVerdict> {
    let error_text = format!("{error:#}");
    let mut result = degraded_result(profile, &error_text);
    let mut verdict = result.verdict();
    let structured = output.is_structured();

    if !structured {
        let caps = render::stdout_capabilities();
        output.report(render_report(
            &result.findings,
            &result.inventory,
            verdict,
            caps,
        ));
    }

    let (summary, repairs) = offer_fix(None, profile, &output, &result.findings).await?;
    if summary.attempted > 0 {
        result = degraded_result(profile, &error_text);
        verdict = result.verdict();
    }

    if structured {
        result.repairs = repairs;
        output.emit_result(ResultVerdict::from(verdict), result)?;
    } else if summary.attempted > 0 {
        output.narrate("");
        output.narrate(format!(
            "Repairs complete: {} attempted, {} failed. Final state: {}.",
            summary.attempted,
            summary.failed,
            doctor_verdict_label(verdict)
        ));
    }
    Ok(verdict)
}

fn degraded_result(profile: &Profile, error: &str) -> DoctorResult {
    let inventory = Inventory {
        home: profile.root().to_path_buf(),
        durable_revision: None,
        serving_revision: None,
        daemon: DaemonFacts {
            status: None,
            probe: DaemonProbe::Unreachable {
                message: error.to_owned(),
            },
        },
        filesystems: Vec::new(),
        mounts: Vec::new(),
    };
    let mut findings = vec![Finding {
        section: Section::Profile,
        check: Check::DaemonIdentity,
        target: None,
        severity: Severity::Failure,
        message: format!("daemon failed to start: {error}"),
        fix: None,
        remediation: None,
    }];
    findings.extend(degraded_bootstrap_findings(profile));
    if let Some(finding) = stale_process_identity_finding(&inventory.daemon.probe, profile) {
        findings.push(finding);
    }
    DoctorResult {
        inventory,
        findings,
        repairs: Vec::new(),
    }
}

fn degraded_bootstrap_findings(profile: &Profile) -> Vec<Finding> {
    let identity = match profile.read_process_identity() {
        Ok(Some(_)) => Finding {
            section: Section::Profile,
            check: Check::DaemonIdentity,
            target: None,
            severity: Severity::Neutral,
            message: "daemon process identity is present".to_owned(),
            fix: None,
            remediation: None,
        },
        Ok(None) => Finding {
            section: Section::Profile,
            check: Check::DaemonIdentity,
            target: None,
            severity: Severity::Neutral,
            message: "daemon process identity is absent".to_owned(),
            fix: None,
            remediation: None,
        },
        Err(error) => Finding {
            section: Section::Profile,
            check: Check::DaemonIdentity,
            target: None,
            severity: Severity::Attention,
            message: format!("read daemon process identity: {error:#}"),
            fix: None,
            remediation: None,
        },
    };
    let socket = if profile.control_socket().exists() {
        Finding {
            section: Section::Profile,
            check: Check::DaemonIdentity,
            target: None,
            severity: Severity::Attention,
            message: "daemon control socket exists but did not become ready".to_owned(),
            fix: None,
            remediation: None,
        }
    } else {
        Finding {
            section: Section::Profile,
            check: Check::DaemonIdentity,
            target: None,
            severity: Severity::Neutral,
            message: "daemon control socket is absent".to_owned(),
            fix: None,
            remediation: None,
        }
    };
    vec![identity, socket]
}

fn repair_from_outcome(outcome: DoctorRepairOutcome) -> Repair {
    match outcome.state {
        DoctorRepairState::Applied => Repair::applied(outcome.command_line),
        DoctorRepairState::Failed => Repair::failed(
            outcome.command_line,
            outcome
                .error
                .unwrap_or_else(|| "daemon remediation failed".to_owned()),
        ),
        DoctorRepairState::Skipped => Repair::skipped(outcome.command_line),
    }
}

fn daemon_command(remediations: &[Remediation], id: &str) -> String {
    remediations
        .iter()
        .find_map(|remediation| match remediation {
            Remediation::DaemonExecuted {
                id: remediation_id,
                command_line,
            } if remediation_id == id => Some(command_line.clone()),
            _ => None,
        })
        .unwrap_or_else(|| "omnifs doctor".to_owned())
}

async fn daemon_repair_outcomes(
    rpc: Option<&RpcClient>,
    remediations: &[Remediation],
) -> (Vec<String>, Vec<DoctorRepairOutcome>) {
    let ids: Vec<String> = remediations
        .iter()
        .filter_map(|remediation| match remediation {
            Remediation::DaemonExecuted { id, .. } => Some(id.clone()),
            Remediation::MountReauth(_) | Remediation::CleanStaleInstance { .. } => None,
        })
        .collect();
    if ids.is_empty() {
        return (ids, Vec::new());
    }
    let fallback = |message: String| {
        ids.iter()
            .map(|id| DoctorRepairOutcome {
                id: id.clone(),
                command_line: daemon_command(remediations, id),
                state: DoctorRepairState::Failed,
                error: Some(message.clone()),
            })
            .collect()
    };
    let outcomes = match rpc {
        Some(rpc) => match rpc.apply_doctor_repairs(&ids).await {
            Ok(outcomes) => outcomes,
            Err(error) => fallback(format!("{error:#}")),
        },
        None => fallback("daemon remediation unavailable in degraded mode".to_owned()),
    };
    (ids, outcomes)
}

fn apply_local_remediation(
    remediation: Remediation,
    profile: &Profile,
    structured: bool,
    summary: &mut RepairSummary,
) -> Repair {
    match remediation {
        Remediation::MountReauth(name) => {
            let command_line = format!("omnifs mount reauth {name}");
            if structured {
                return Repair::skipped(command_line);
            }
            summary.attempted += 1;
            match Remediation::MountReauth(name).apply(profile) {
                Ok(()) => Repair::applied(command_line),
                Err(error) => {
                    summary.failed += 1;
                    Repair::failed(command_line, format!("{error:#}"))
                },
            }
        },
        remediation @ Remediation::CleanStaleInstance { .. } => {
            let command_line = remediation.command_line();
            summary.attempted += 1;
            match remediation.apply(profile) {
                Ok(()) => Repair::applied(command_line),
                Err(error) => {
                    summary.failed += 1;
                    Repair::failed(command_line, format!("{error:#}"))
                },
            }
        },
        Remediation::DaemonExecuted { .. } => Repair::failed(
            remediation.command_line(),
            "daemon remediation was not batched".to_owned(),
        ),
    }
}

/// Show the complete repair set, ask once, then continue through independent
/// failures. Client mount reauth stays local; daemon remediations are sent as
/// one control-plane batch. Structured mode leaves mount reauth for the
/// caller, while --yes authorizes daemon repairs without a prompt.
async fn offer_fix(
    rpc: Option<&RpcClient>,
    profile: &Profile,
    output: &Output,
    findings: &[Finding],
) -> anyhow::Result<(RepairSummary, Vec<Repair>)> {
    let remediations = remediable_fixes(findings);
    let structured = output.is_structured();
    if remediations.is_empty() || output.no_input() {
        return Ok((RepairSummary::default(), Vec::new()));
    }
    if structured {
        if !output.yes() {
            return Ok((RepairSummary::default(), Vec::new()));
        }
    } else {
        if !output.yes() && !crate::ui::prompt::is_terminal() {
            return Ok((RepairSummary::default(), Vec::new()));
        }
        output.narrate("");
        output.narrate("Repairs:");
        for remediation in &remediations {
            output.narrate(format!("  - {}", remediation.command_line()));
        }
        if !output.yes()
            && !Confirm::new(format!("Apply all {} repairs now?", remediations.len()))
                .ask_with_output(output)?
        {
            return Ok((RepairSummary::default(), Vec::new()));
        }
    }

    let caps = render::stdout_capabilities();
    let ledger_rows: Vec<LedgerRow> = remediations
        .iter()
        .map(|remediation| LedgerRow::new(Glyph::Done, "fix", remediation.command_line()))
        .collect();
    let key_width = render::ledger_key_width(&ledger_rows);
    let (daemon_ids, daemon_outcomes) = daemon_repair_outcomes(rpc, &remediations).await;

    let mut summary = RepairSummary {
        attempted: daemon_ids.len(),
        failed: daemon_outcomes
            .iter()
            .filter(|outcome| outcome.state == DoctorRepairState::Failed)
            .count(),
    };
    let mut repairs = Vec::with_capacity(remediations.len());
    let mut outcomes = daemon_outcomes
        .into_iter()
        .map(|outcome| (outcome.id.clone(), outcome))
        .collect::<BTreeMap<_, _>>();
    for (remediation, mut ledger_row) in remediations.into_iter().zip(ledger_rows) {
        let repair = match remediation {
            Remediation::DaemonExecuted { id, .. } => {
                let outcome = outcomes.remove(&id).unwrap_or(DoctorRepairOutcome {
                    id,
                    command_line: ledger_row.value.clone(),
                    state: DoctorRepairState::Failed,
                    error: Some("daemon returned no repair outcome".to_owned()),
                });
                repair_from_outcome(outcome)
            },
            remediation => apply_local_remediation(remediation, profile, structured, &mut summary),
        };
        if !structured {
            if repair.state == RepairState::Failed {
                ledger_row.glyph = Glyph::Fail;
                let error = repair.error.as_deref().unwrap_or("repair failed");
                ledger_row.value = format!("{}: {error}", ledger_row.value);
            }
            output.report(format!(
                "{}\n",
                render::ledger_row_line(&ledger_row, key_width, caps)
            ));
        }
        repairs.push(repair);
    }
    Ok((summary, repairs))
}

const fn doctor_verdict_label(verdict: DoctorVerdict) -> &'static str {
    match verdict {
        DoctorVerdict::Clean => "clean",
        DoctorVerdict::Warnings => "warnings remain",
        DoctorVerdict::Failures => "failures remain",
    }
}

#[cfg(test)]
mod golden {
    use super::*;
    use crate::ui::output::OutputMode;
    use omnifs_api::DoctorRemediation;
    use tempfile::TempDir;

    fn caps(color: bool) -> Capabilities {
        Capabilities { width: 120, color }
    }

    fn api_finding(
        section: DoctorSection,
        check: DoctorCheckKind,
        target: Option<&str>,
        severity: DoctorSeverity,
        message: &str,
        fix: Option<&str>,
        remediation_id: Option<&str>,
    ) -> DoctorFinding {
        DoctorFinding {
            section,
            check,
            target: target.map(str::to_owned),
            severity,
            message: message.to_owned(),
            fix: fix.map(str::to_owned),
            remediation_id: remediation_id.map(str::to_owned),
        }
    }

    fn api_report(
        findings: Vec<DoctorFinding>,
        remediations: Vec<DoctorRemediation>,
    ) -> RunDoctorReport {
        RunDoctorReport {
            findings,
            remediations,
        }
    }

    fn probes() -> Vec<Finding> {
        let report = api_report(
            vec![
                api_finding(
                    DoctorSection::Environment,
                    DoctorCheckKind::Docker,
                    None,
                    DoctorSeverity::Positive,
                    "docker daemon responds",
                    None,
                    None,
                ),
                api_finding(
                    DoctorSection::Environment,
                    DoctorCheckKind::Fuse,
                    None,
                    DoctorSeverity::Neutral,
                    "macOS: native mount is NFS loopback",
                    None,
                    None,
                ),
                api_finding(
                    DoctorSection::Environment,
                    DoctorCheckKind::Network,
                    None,
                    DoctorSeverity::Positive,
                    "ghcr.io reachable",
                    None,
                    None,
                ),
                api_finding(
                    DoctorSection::Profile,
                    DoctorCheckKind::Config,
                    None,
                    DoctorSeverity::Positive,
                    "defaults (~/.omnifs/config.toml absent)",
                    None,
                    None,
                ),
            ],
            Vec::new(),
        );
        findings_from_report(&report)
    }

    fn targeted_finding() -> Finding {
        let report = api_report(
            vec![api_finding(
                DoctorSection::Mounts,
                DoctorCheckKind::Credentials,
                Some("github"),
                DoctorSeverity::Attention,
                "token expired",
                Some("omnifs mount reauth github"),
                Some("mount-github"),
            )],
            vec![DoctorRemediation {
                id: "mount-github".to_owned(),
                command_line: "omnifs mount reauth github".to_owned(),
                executor: DoctorExecutor::ClientMountReauth {
                    mount: "github".to_owned(),
                },
            }],
        );
        findings_from_report(&report).pop().unwrap()
    }

    fn daemon_finding() -> Finding {
        let report = api_report(
            vec![api_finding(
                DoctorSection::Filesystems,
                DoctorCheckKind::StrayFilesystem,
                Some("docker"),
                DoctorSeverity::Attention,
                "stray filesystem",
                Some("omnifs fs rm docker"),
                Some("remove-docker"),
            )],
            vec![DoctorRemediation {
                id: "remove-docker".to_owned(),
                command_line: "omnifs fs rm docker".to_owned(),
                executor: DoctorExecutor::Daemon,
            }],
        );
        findings_from_report(&report).pop().unwrap()
    }

    fn running_inventory() -> Inventory {
        Inventory::test(DaemonHealth::Running, Vec::new(), Vec::new())
    }

    #[test]
    fn healthy_checklist_ends_with_everything_checks_out() {
        let findings = probes();
        let inventory = running_inventory();
        let rendered = render_report(&findings, &inventory, DoctorVerdict::Clean, caps(false));
        assert!(rendered.contains("Environment"), "{rendered}");
        assert!(rendered.contains("Profile"), "{rendered}");
        assert!(rendered.contains("Mounts"), "{rendered}");
        assert!(rendered.contains("Filesystems"), "{rendered}");
        assert!(rendered.contains("Daemon"), "{rendered}");
        assert!(
            rendered.trim_end().ends_with("Everything checks out."),
            "{rendered}"
        );
        assert!(rendered.contains("\n\nProfile\n"), "{rendered}");
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
        assert!(
            !lines[credentials_index + 1].trim().starts_with("fix:"),
            "{rendered}"
        );
        assert!(rendered.contains("  ✓ docker"), "{rendered}");
        assert!(rendered.contains("  • fuse"), "{rendered}");
        assert!(rendered.contains("  ✓ running"), "{rendered}");
        assert_eq!(
            lines.last().copied().unwrap_or_default(),
            "1 warning. Fix it:  omnifs mount reauth github"
        );
    }

    #[test]
    fn verdict_omits_a_fix_unless_every_problem_shares_it() {
        let warning = |fix: Option<&str>| Row {
            severity: Severity::Attention,
            key: "check".to_owned(),
            value: "needs attention".to_owned(),
            fix: fix.map(str::to_owned),
        };
        let first = warning(Some("omnifs status"));
        let different = warning(Some("omnifs doctor"));
        assert_eq!(
            verdict_line(&[&first, &different], DoctorVerdict::Warnings, caps(false)),
            "2 warnings."
        );
        let missing = warning(None);
        assert_eq!(
            verdict_line(&[&first, &missing], DoctorVerdict::Warnings, caps(false)),
            "2 warnings."
        );
    }

    #[test]
    fn duplicate_repairs_are_offered_once() {
        let finding = targeted_finding();
        let fixes = remediable_fixes(&[finding.clone(), finding]);
        assert_eq!(fixes.len(), 1);
        assert_eq!(fixes[0].command_line(), "omnifs mount reauth github");
    }

    #[test]
    fn verdict_combines_inventory_with_maximum_finding_severity() {
        let clean = DoctorResult {
            inventory: Inventory::test(DaemonHealth::Stopped, Vec::new(), Vec::new()),
            findings: Vec::new(),
            repairs: Vec::new(),
        };
        assert_eq!(clean.verdict(), DoctorVerdict::Clean);

        let degraded = DoctorResult {
            inventory: Inventory::test(DaemonHealth::Failed, Vec::new(), Vec::new()),
            findings: Vec::new(),
            repairs: Vec::new(),
        };
        assert_eq!(degraded.verdict(), DoctorVerdict::Warnings);

        let mut warnings = clean.clone();
        warnings.findings.push(targeted_finding());
        assert_eq!(warnings.verdict(), DoctorVerdict::Warnings);

        let mut failures = warnings;
        failures.findings.push(Finding {
            section: Section::Environment,
            check: Check::Docker,
            target: None,
            severity: Severity::Failure,
            message: "failed to load".to_owned(),
            fix: Some("omnifs logs".to_owned()),
            remediation: None,
        });
        assert_eq!(failures.verdict(), DoctorVerdict::Failures);
    }

    #[test]
    fn doctor_json_preserves_inventory_and_findings_and_skips_presentation_only_fields() {
        let payload = DoctorResult {
            inventory: Inventory::test(DaemonHealth::Stopped, Vec::new(), Vec::new()),
            findings: vec![targeted_finding()],
            repairs: Vec::new(),
        };
        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string_pretty(&payload).unwrap()).unwrap();
        assert!(value.get("inventory").is_some());
        assert_eq!(value["findings"][0]["check"], "credentials");
        assert_eq!(value["findings"][0]["target"], "github");
        assert_eq!(value["findings"][0]["severity"], "attention");
        assert_eq!(value["findings"][0]["fix"], "omnifs mount reauth github");
        assert!(value["findings"][0].get("section").is_none());
        assert!(value["findings"][0].get("remediation").is_none());
        assert!(value.get("repairs").is_none());
    }

    #[test]
    fn doctor_json_includes_repairs_when_present() {
        let payload = DoctorResult {
            inventory: Inventory::test(DaemonHealth::Stopped, Vec::new(), Vec::new()),
            findings: Vec::new(),
            repairs: vec![
                Repair::applied("omnifs fs rm docker".to_owned()),
                Repair::failed(
                    "omnifs doctor (clean stale daemon identity)".to_owned(),
                    "boom".to_owned(),
                ),
                Repair::skipped("omnifs mount reauth github".to_owned()),
            ],
        };
        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string_pretty(&payload).unwrap()).unwrap();
        assert_eq!(value["repairs"][0]["state"], "applied");
        assert!(value["repairs"][0].get("error").is_none());
        assert_eq!(value["repairs"][1]["state"], "failed");
        assert_eq!(value["repairs"][1]["error"], "boom");
        assert_eq!(value["repairs"][2]["state"], "skipped");
    }

    #[test]
    fn remediable_fixes_returns_the_actionable_subset() {
        let all_remediable = vec![targeted_finding()];
        assert_eq!(remediable_fixes(&all_remediable).len(), 1);
        let mixed = vec![
            targeted_finding(),
            Finding {
                section: Section::Environment,
                check: Check::Network,
                target: None,
                severity: Severity::Attention,
                message: "ghcr.io unreachable".to_owned(),
                fix: None,
                remediation: None,
            },
        ];
        assert_eq!(remediable_fixes(&mixed).len(), 1);
        let daemon = daemon_finding();
        assert!(matches!(
            remediable_fixes(&[daemon])[0],
            Remediation::DaemonExecuted { .. }
        ));
        assert!(remediable_fixes(&probes()).is_empty());
    }

    #[test]
    fn daemon_report_maps_remediation_executor_and_wire_fields() {
        let finding = daemon_finding();
        assert_eq!(finding.section, Section::Filesystems);
        assert_eq!(finding.check, Check::StrayFilesystem);
        assert!(matches!(
            finding.remediation,
            Some(Remediation::DaemonExecuted { ref id, .. }) if id == "remove-docker"
        ));
    }

    #[test]
    fn dead_process_identity_becomes_a_doctor_remediation() {
        let root = TempDir::new().unwrap();
        let endpoint = Profile::under_root(root.path());
        std::fs::write(
            endpoint.process_identity_path(),
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "pid": std::process::id(),
                "instance_token": "stale",
                "executable": std::env::current_exe().unwrap(),
                "start_identity": "not-the-current-process"
            }))
            .unwrap(),
        )
        .unwrap();

        let finding = stale_process_identity_finding(
            &DaemonProbe::Unreachable {
                message: "connection refused".to_owned(),
            },
            &endpoint,
        )
        .expect("stale identity finding");
        assert!(matches!(
            finding.remediation,
            Some(Remediation::CleanStaleInstance { .. })
        ));
    }

    #[tokio::test]
    async fn degraded_path_reports_start_error_without_control_client() {
        let root = TempDir::new().unwrap();
        let profile = Profile::under_root(root.path());
        let output = Output::new(OutputMode::Json, false).with_yes(true);
        let verdict = run_degraded_at(anyhow::anyhow!("synthetic start failure"), &profile, output)
            .await
            .unwrap();
        assert_eq!(verdict, DoctorVerdict::Failures);
    }
}
