//! `omnifs doctor` — runtime + auth diagnostics, presented as a grouped
//! checklist. It can run mount reauth and narrowly proved filesystem cleanup.
//! Reauth is a fresh `omnifs mount reauth <name>` subprocess rather than a
//! call into `commands::mount`'s internal API.

use anyhow::Context as _;
use omnifs_bootstrap::Profile;
use omnifs_core::fs;
use omnifs_fs_runtime::{
    Candidate, DockerClient, DockerTarget, HostDriver, ImageInspection, ImageRef, LibkrunRunner,
    OwnedFilesystemContainer, RuntimeEventSink, RuntimePaths, owned_filesystems,
};
use omnifs_state::DaemonStatePaths;
use serde::Serialize;
use std::path::{Path, PathBuf};

use crate::inventory::{AuthState, DaemonHealth, DaemonProbe, Inventory, MountStatus, Severity};
use crate::legacy_filesystems::LegacyFilesystems;
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
    let profile = Profile::resolve()?;
    let legacy_filesystems = LegacyFilesystems::under_profile(profile.root());
    let daemon_runtime_paths = daemon_runtime_paths(&profile)?;
    let inventory = Inventory::collect_rpc().await?;
    let docker_target = resolve_filesystem_target(&legacy_filesystems)
        .map_err(|error: anyhow::Error| format!("resolve target: {error:#}"));
    Doctor {
        profile,
        legacy_filesystems,
        daemon_runtime_paths,
        inventory,
        docker_target,
        output,
    }
    .run()
    .await
}

/// The optional Docker-hosted FUSE filesystem's target, probed by the
/// `docker reachable`/`image cached` diagnostics. The daemon itself always
/// runs host-native, so there is no daemon Docker target to resolve here.
fn resolve_filesystem_target(
    legacy_filesystems: &LegacyFilesystems,
) -> anyhow::Result<DockerTarget> {
    let id = legacy_filesystems
        .scan()?
        .specs
        .into_iter()
        .find(|spec| spec.runtime() == fs::Runtime::Docker)
        .map_or_else(
            || fs::Id::new("doctor").expect("static filesystem id"),
            |spec| spec.id().clone(),
        );
    let paths = legacy_filesystems.runtime_paths()?;
    DockerTarget::for_filesystem(paths.profile_root(), paths.is_default_profile(), &id, None)
}

fn daemon_runtime_paths(profile: &Profile) -> anyhow::Result<RuntimePaths> {
    let state = DaemonStatePaths::new(profile.root().join("daemon-state"));
    Ok(RuntimePaths::daemon_owned(
        profile.root().to_path_buf(),
        std::env::var_os(omnifs_bootstrap::OMNIFS_HOME_ENV).is_none(),
        state.attachments_runtime(),
        state.attachment_logs(),
        state.guest_images_cache(),
        std::env::current_exe().context("resolve the omnifs executable")?,
    ))
}

struct Doctor {
    profile: Profile,
    legacy_filesystems: LegacyFilesystems,
    daemon_runtime_paths: RuntimePaths,
    inventory: Inventory,
    /// The filesystem's Docker target, or the error resolving it.
    docker_target: Result<DockerTarget, String>,
    output: Output,
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
    #[serde(rename = "legacy filesystem spec")]
    LegacyFilesystemSpec,
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
            Self::LegacyFilesystemSpec => "legacy filesystem spec",
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
    CleanStaleInstance {
        identity: omnifs_bootstrap::DaemonIdentity,
    },
    StopHostFilesystem {
        paths: RuntimePaths,
        state_dir: PathBuf,
        record: omnifs_mtab::RunnerRecord,
    },
    CleanStaleHostRecord {
        paths: RuntimePaths,
        state_dir: PathBuf,
        record: omnifs_mtab::RunnerRecord,
    },
    StopLibkrunFilesystem {
        state_dir: PathBuf,
        record: omnifs_libkrun::HelperRecord,
    },
}

impl Remediation {
    fn command_line(&self) -> String {
        match self {
            Self::MountReauth(name) => format!("omnifs mount reauth {name}"),
            Self::CleanStaleInstance { .. } => {
                "omnifs doctor (clean stale daemon identity)".to_owned()
            },
            Self::StopHostFilesystem { record, .. } => {
                format!("omnifs fs detach --name {}", record.spec.id())
            },
            Self::CleanStaleHostRecord { record, .. } => {
                format!(
                    "omnifs doctor (clean stale host record for {})",
                    record.spec.location().display()
                )
            },
            Self::StopLibkrunFilesystem { record, .. } => {
                format!("omnifs fs detach --name {}", record.spec.id())
            },
        }
    }

    /// Spawn the fresh subprocess and require it to exit successfully. Array
    /// arguments only, never a shell string: the mount name came from the
    /// already-collected inventory, not from re-parsing the advisory `fix`
    /// text.
    async fn apply(&self, profile: &Profile) -> anyhow::Result<()> {
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
            Self::StopHostFilesystem {
                paths,
                state_dir,
                record,
            } => {
                let _guard = acquire_stopped_daemon_guard(profile).await?;
                let paths = paths.attachment(record.spec.id());
                HostDriver::new(
                    state_dir.clone(),
                    paths.host_log().to_path_buf(),
                    paths.executable().to_path_buf(),
                    RuntimeEventSink::discard(),
                )
                .stop_confirmed(record)
                .await
            },
            Self::CleanStaleHostRecord {
                paths,
                state_dir,
                record,
            } => {
                let _guard = acquire_stopped_daemon_guard(profile).await?;
                let paths = paths.attachment(record.spec.id());
                HostDriver::new(
                    state_dir.clone(),
                    paths.host_log().to_path_buf(),
                    paths.executable().to_path_buf(),
                    RuntimeEventSink::discard(),
                )
                .cleanup_stale(record)
                .await
            },
            Self::StopLibkrunFilesystem { state_dir, record } => {
                let _guard = acquire_stopped_daemon_guard(profile).await?;
                LibkrunRunner::new(state_dir.clone())
                    .stop_confirmed(record.clone())
                    .await
            },
        }
    }
}

async fn acquire_stopped_daemon_guard(
    profile: &Profile,
) -> anyhow::Result<omnifs_bootstrap::SpawnLock> {
    let guard = profile
        .acquire_spawn_lock()
        .context("acquire daemon spawn lock")?;
    anyhow::ensure!(
        profile.read_process_identity()?.is_none(),
        "daemon has a process identity; refusing filesystem teardown"
    );
    match tokio::net::UnixStream::connect(profile.control_socket()).await {
        Ok(_) => anyhow::bail!("daemon is running; refusing filesystem teardown"),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) => {},
        Err(error) => return Err(error).context("probe daemon control socket before teardown"),
    }
    anyhow::ensure!(
        profile.read_process_identity()?.is_none(),
        "daemon started during the safety check; refusing filesystem teardown"
    );
    Ok(guard)
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
    fn from_probe(
        section: Section,
        check: Check,
        target: Option<String>,
        result: ProbeResult,
    ) -> Self {
        let (severity, message) = result.into_parts();
        Self {
            section,
            check,
            target,
            severity,
            message,
            fix: None,
            remediation: None,
        }
    }

    /// One finding per mount whose credential needs attention, built entirely from data `Inventory`
    /// already collected: doctor invents no new auth check here.
    fn mount_auth(mount: &MountStatus) -> Option<Self> {
        let (message, command) = match &mount.auth {
            AuthState::Missing { command } => ("credential missing".to_owned(), command.clone()),
            AuthState::Expired { command } => ("token expired".to_owned(), command.clone()),
            AuthState::Error { message, command } => (message.clone(), command.clone()),
            AuthState::NotNeeded | AuthState::Ready => return None,
        };
        Some(Self {
            section: Section::Mounts,
            check: Check::Credentials,
            target: Some(mount.name.clone()),
            severity: mount.auth.severity(),
            message,
            fix: Some(command),
            remediation: Some(Remediation::MountReauth(mount.name.clone())),
        })
    }
}

/// The check a backend's ownership-scan failure reports under, shared by a
/// whole-backend listing failure (`Candidate::ListingFailed`) and one
/// unreadable scan entry (`Candidate::Invalid`).
fn ownership_check_for(backend: &str) -> Check {
    match backend {
        "docker" => Check::DockerFilesystemOwnership,
        "libkrun" => Check::LibkrunFilesystemOwnership,
        _ => Check::FilesystemState,
    }
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
            Self::Err(message) => (Severity::Failure, message),
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
async fn offer_fix(
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
    let mut summary = RepairSummary::default();
    let mut repairs = Vec::with_capacity(remediations.len());
    for (remediation, mut ledger_row) in remediations.into_iter().zip(ledger_rows) {
        let command_line = remediation.command_line();
        if structured && matches!(remediation, Remediation::MountReauth(_)) {
            repairs.push(Repair::skipped(command_line));
            continue;
        }
        summary.attempted += 1;
        let outcome = remediation.apply(profile).await;
        match &outcome {
            Ok(()) => repairs.push(Repair::applied(command_line)),
            Err(error) => {
                summary.failed += 1;
                repairs.push(Repair::failed(command_line, format!("{error:#}")));
            },
        }
        if !structured {
            if let Err(error) = &outcome {
                ledger_row.glyph = Glyph::Fail;
                ledger_row.value = format!("{}: {error:#}", ledger_row.value);
            }
            output.report(format!(
                "{}\n",
                render::ledger_row_line(&ledger_row, key_width, caps)
            ));
        }
    }
    Ok((summary, repairs))
}

impl Doctor {
    /// Diagnose, offer repairs (mode-aware inside [`offer_fix`]), and
    /// re-diagnose once if anything was attempted. Human mode prints the
    /// report before repairs run, exactly as it always has; structured mode
    /// never prints, folding repairs into the one JSON result instead.
    async fn run(self) -> anyhow::Result<DoctorVerdict> {
        let mut result = self.diagnose().await?;
        let mut verdict = result.verdict();
        let structured = self.output.is_structured();

        if !structured {
            let caps = render::stdout_capabilities();
            self.output.report(render_report(
                &result.findings,
                &result.inventory,
                verdict,
                caps,
            ));
        }

        let (summary, repairs) = offer_fix(&self.profile, &self.output, &result.findings).await?;
        if summary.attempted > 0 {
            result = self.rediagnose_after_repairs().await?;
            verdict = result.verdict();
        }

        if structured {
            result.repairs = repairs;
            self.output
                .emit_result(ResultVerdict::from(verdict), result)?;
            return Ok(verdict);
        }
        if summary.attempted > 0 {
            self.output.narrate("");
            self.output.narrate(format!(
                "Repairs complete: {} attempted, {} failed. Final state: {}.",
                summary.attempted,
                summary.failed,
                doctor_verdict_label(verdict)
            ));
        }
        Ok(verdict)
    }

    /// Collect a fresh `Doctor` over the same client state and re-diagnose,
    /// after a repair pass may have changed what the checklist would find.
    async fn rediagnose_after_repairs(&self) -> anyhow::Result<DoctorResult> {
        let fresh = Doctor {
            profile: self.profile.clone(),
            legacy_filesystems: self.legacy_filesystems.clone(),
            daemon_runtime_paths: self.daemon_runtime_paths.clone(),
            inventory: Inventory::collect_rpc().await?,
            docker_target: resolve_filesystem_target(&self.legacy_filesystems)
                .map_err(|error: anyhow::Error| format!("resolve target: {error:#}")),
            output: self.output.clone(),
        };
        fresh.diagnose().await
    }

    async fn diagnose(&self) -> anyhow::Result<DoctorResult> {
        let (runtime, mut findings) = self.base_findings().await;
        findings.extend(self.inventory.mounts.iter().filter_map(Finding::mount_auth));
        let daemon_health = self.inventory.daemon.health();
        findings.extend(
            self.filesystem_findings(runtime.as_ref(), daemon_health)
                .await,
        );

        Ok(DoctorResult {
            inventory: self.inventory.clone(),
            findings,
            repairs: Vec::new(),
        })
    }

    async fn base_findings(&self) -> (Option<DockerClient>, Vec<Finding>) {
        let mut findings = Vec::new();
        if let Ok(endpoint) = Profile::resolve()
            && let Some(finding) =
                stale_process_identity_finding(&self.inventory.daemon.probe, &endpoint)
        {
            findings.push(finding);
        }
        let (runtime, docker_result) = self.probe_docker_reachable().await;
        let docker_ok = matches!(docker_result, ProbeResult::Ok(_));
        findings.push(Finding::from_probe(
            Section::Environment,
            Check::Docker,
            None,
            docker_result,
        ));
        findings.push(Finding::from_probe(
            Section::Environment,
            Check::Fuse,
            None,
            Self::probe_fuse(),
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
            Check::Image,
            None,
            image_result,
        ));
        for (check, result) in [
            (Check::CredentialStore, self.probe_credential_store()),
            (Check::SshAgent, Self::probe_ssh_agent()),
            (Check::Config, self.probe_config_file()),
        ] {
            findings.push(Finding::from_probe(Section::Profile, check, None, result));
        }
        findings.extend(self.legacy_spec_findings());
        findings.push(Finding::from_probe(
            Section::Environment,
            Check::Network,
            None,
            self.probe_network().await,
        ));
        (runtime, findings)
    }

    fn attached(&self, spec: &fs::Spec) -> bool {
        self.inventory
            .filesystems
            .iter()
            .any(|filesystem| filesystem.spec == *spec)
    }

    /// One generic pass over every backend's owned-instance scan: a
    /// candidate confirmed live but unattached becomes a stray-filesystem
    /// finding, and a candidate the backend could not confirm becomes its
    /// own error finding. Each backend still resolves and confirms its
    /// candidates its own way (a Docker container needs a second connection
    /// and identity re-check no on-disk record needs), so this dispatches
    /// one small per-backend helper per candidate rather than forcing every
    /// backend's genuinely different confirmation shape into one signature.
    async fn filesystem_findings(
        &self,
        docker: Option<&DockerClient>,
        daemon_health: DaemonHealth,
    ) -> Vec<Finding> {
        let mut findings = self
            .filesystem_findings_at(&self.daemon_runtime_paths, docker, daemon_health)
            .await;
        let legacy_paths = match self.legacy_filesystems.runtime_paths() {
            Ok(paths) => paths,
            Err(error) => {
                findings.push(Finding::from_probe(
                    Section::Filesystems,
                    Check::FilesystemState,
                    None,
                    ProbeResult::Err(format!("{error:#}")),
                ));
                return findings;
            },
        };
        findings.extend(
            self.filesystem_findings_at(&legacy_paths, None, daemon_health)
                .await,
        );
        findings
    }

    async fn filesystem_findings_at(
        &self,
        paths: &RuntimePaths,
        docker: Option<&DockerClient>,
        daemon_health: DaemonHealth,
    ) -> Vec<Finding> {
        let candidates = owned_filesystems(paths, docker).await;
        let mut findings = Vec::new();
        for candidate in candidates {
            match candidate {
                Candidate::ListingFailed { backend, error } => {
                    findings.push(Finding::from_probe(
                        Section::Filesystems,
                        ownership_check_for(backend),
                        None,
                        ProbeResult::Err(error),
                    ));
                },
                Candidate::Invalid {
                    backend,
                    target,
                    error,
                } => {
                    findings.push(Finding::from_probe(
                        Section::Filesystems,
                        ownership_check_for(backend),
                        target,
                        ProbeResult::Err(error),
                    ));
                },
                Candidate::Host {
                    state_dir,
                    record,
                    confirmed,
                } => match self.host_candidate_finding(
                    paths,
                    state_dir,
                    record,
                    confirmed,
                    daemon_health,
                ) {
                    Ok(finding) => findings.extend(finding),
                    Err(error) => findings.push(Finding::from_probe(
                        Section::Filesystems,
                        Check::FilesystemState,
                        None,
                        ProbeResult::Err(format!("{error:#}")),
                    )),
                },
                Candidate::Docker(owned) => {
                    findings.extend(Self::docker_candidate_finding(owned, daemon_health));
                },
                Candidate::Libkrun {
                    id,
                    state_dir,
                    confirmed,
                } => {
                    findings.extend(self.libkrun_candidate_finding(
                        &id,
                        state_dir,
                        confirmed,
                        daemon_health,
                    ));
                },
            }
        }
        findings
    }

    /// One host runner candidate: `Ok(None)` when it needs no finding
    /// (confirmed and attached), an error only when proving the mount's
    /// active state itself fails (the runner control probe's own failure is
    /// reported as a finding, not propagated). An unreadable candidate is
    /// handled by the shared `Candidate::Invalid` arm before this is ever
    /// called, so this only ever sees a runner that was actually read.
    fn host_candidate_finding(
        &self,
        paths: &RuntimePaths,
        state_dir: PathBuf,
        record: omnifs_mtab::RunnerRecord,
        confirmed: Result<omnifs_thin::host_control::RunnerPhase, String>,
        daemon_health: DaemonHealth,
    ) -> anyhow::Result<Option<Finding>> {
        let spec = record.spec.clone();
        let mount_point = spec.location().to_path_buf();
        let is_attached = self.attached(&spec);
        let target = Some(format!(
            "`{}` {}/host at {}",
            spec.id(),
            spec.protocol(),
            mount_point.display()
        ));
        match confirmed {
            Ok(_) if is_attached => Ok(None),
            Ok(phase) => {
                let remediation = (daemon_health == DaemonHealth::Stopped).then_some(
                    Remediation::StopHostFilesystem {
                        paths: paths.clone(),
                        state_dir,
                        record,
                    },
                );
                Ok(Some(Finding {
                    section: Section::Filesystems,
                    check: Check::StrayFilesystem,
                    target,
                    severity: Severity::Attention,
                    message: format!(
                        "runner is confirmed in phase {phase:?} but daemon health is {daemon_health:?} and reports no matching attachment"
                    ),
                    fix: remediation.as_ref().map(Remediation::command_line),
                    remediation,
                }))
            },
            Err(error) => {
                let mount_active = omnifs_nfs::mount_is_active_checked(&mount_point)?;
                let remediation =
                    (!mount_active && !is_attached).then_some(Remediation::CleanStaleHostRecord {
                        paths: paths.clone(),
                        state_dir,
                        record,
                    });
                Ok(Some(Finding {
                    section: Section::Filesystems,
                    check: Check::StaleFilesystemState,
                    target,
                    severity: if mount_active || is_attached {
                        Severity::Failure
                    } else {
                        Severity::Attention
                    },
                    message: if is_attached {
                        format!(
                            "runner control cannot be confirmed but the daemon still reports it attached: {error}"
                        )
                    } else if mount_active {
                        format!("runner cannot be confirmed but its mount is active: {error}")
                    } else {
                        format!("runner cannot be confirmed: {error}")
                    },
                    fix: remediation.as_ref().map(Remediation::command_line),
                    remediation,
                }))
            },
        }
    }

    /// A Docker scan proves only the immutable container ID and filesystem
    /// label. It cannot prove the daemon-owned attachment spec or
    /// runtime-instance ID required by the current runtime API, so doctor
    /// must never turn this observation into a stop request.
    fn docker_candidate_finding(
        owned: OwnedFilesystemContainer,
        daemon_health: DaemonHealth,
    ) -> Vec<Finding> {
        vec![Finding {
            section: Section::Filesystems,
            check: Check::DockerFilesystemOwnership,
            target: Some(owned.filesystem_id),
            severity: Severity::Attention,
            message: format!(
                "container {} cannot be remediated automatically: its record has no exact attachment spec or runtime instance (daemon health is {daemon_health:?})",
                owned.identity.id,
            ),
            fix: None,
            remediation: None,
        }]
    }

    /// One libkrun helper candidate, matching
    /// [`Self::host_candidate_finding`] and [`Self::docker_candidate_finding`]'s
    /// shape: 0 or 1 findings, stray filesystem or the reason it could not
    /// be confirmed. An unreadable candidate is handled by the shared
    /// `Candidate::Invalid` arm before this is ever called.
    fn libkrun_candidate_finding(
        &self,
        id: &fs::Id,
        state_dir: PathBuf,
        confirmed: Result<Option<omnifs_libkrun::HelperRecord>, String>,
        daemon_health: DaemonHealth,
    ) -> Vec<Finding> {
        match confirmed {
            Ok(Some(record)) if record.spec.id() != id => {
                vec![Finding::from_probe(
                    Section::Filesystems,
                    Check::LibkrunFilesystemOwnership,
                    Some(id.to_string()),
                    ProbeResult::Err(format!(
                        "helper claims filesystem `{}` instead of matching its state path",
                        record.spec.id()
                    )),
                )]
            },
            Ok(Some(record)) => {
                if self.attached(&record.spec) {
                    return Vec::new();
                }
                let remediation = (daemon_health == DaemonHealth::Stopped)
                    .then_some(Remediation::StopLibkrunFilesystem { state_dir, record });
                vec![Finding {
                    section: Section::Filesystems,
                    check: Check::StrayFilesystem,
                    target: Some(id.to_string()),
                    severity: Severity::Attention,
                    message: format!(
                        "helper identity is confirmed but daemon health is {daemon_health:?} and reports no matching attachment"
                    ),
                    fix: remediation.as_ref().map(Remediation::command_line),
                    remediation,
                }]
            },
            Ok(None) => Vec::new(),
            Err(error) => vec![Finding::from_probe(
                Section::Filesystems,
                Check::LibkrunFilesystemOwnership,
                Some(id.to_string()),
                ProbeResult::Err(error),
            )],
        }
    }

    async fn probe_docker_reachable(&self) -> (Option<DockerClient>, ProbeResult) {
        let target = match &self.docker_target {
            Ok(target) => target,
            Err(error) => return (None, ProbeResult::Err(error.clone())),
        };
        let runtime = match DockerClient::connect_for(target, RuntimeEventSink::discard()) {
            Ok(runtime) => runtime,
            Err(error) => return (None, ProbeResult::Err(format!("connect: {error}"))),
        };
        match runtime.ping().await {
            Ok(()) => (
                Some(runtime),
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
            ProbeResult::Skipped(
                "macOS: native mount is NFS loopback; FUSE runs only inside the optional filesystem container",
            )
        }
    }

    async fn probe_image_cached(&self, runtime: &DockerClient, image: &ImageRef) -> ProbeResult {
        match runtime.inspect_image(image.as_str()).await {
            Ok(ImageInspection::Present) => ProbeResult::Ok(format!("{image} cached")),
            Ok(ImageInspection::Missing) if image.has_registry() => ProbeResult::Warn(format!(
                "{image} not cached (will pull on the next Docker `omnifs fs attach`)"
            )),
            Ok(ImageInspection::Missing) => ProbeResult::Err(format!(
                "{image} not present locally; a dev image is never pulled, so `omnifs fs attach` \
                 cannot start (build it with `just filesystem-image`)"
            )),
            Err(error) => ProbeResult::Err(format!("inspect: {error}")),
        }
    }

    fn probe_credential_store(&self) -> ProbeResult {
        let Some(daemon) = self.inventory.daemon.status.as_ref() else {
            return ProbeResult::Warn("daemon inventory unavailable".into());
        };
        ProbeResult::Ok(format!(
            "{} managed {}",
            crate::ui::render::count(daemon.credentials.len(), "credential"),
            "by daemon"
        ))
    }

    fn probe_ssh_agent() -> ProbeResult {
        match std::env::var_os("SSH_AUTH_SOCK") {
            Some(sock) if Path::new(&sock).exists() => {
                ProbeResult::Ok(Path::new(&sock).display().to_string())
            },
            Some(_) => ProbeResult::Warn("SSH_AUTH_SOCK set but socket not found".into()),
            None => ProbeResult::Warn("SSH_AUTH_SOCK unset; git callouts will fail".into()),
        }
    }

    fn probe_config_file(&self) -> ProbeResult {
        let path = self.legacy_filesystems.profile_root().join("config.toml");
        match crate::profile_config::read(self.legacy_filesystems.profile_root()) {
            Ok(_) if path.exists() => ProbeResult::Ok(path.display().to_string()),
            Ok(_) => ProbeResult::Ok(format!("defaults ({} absent)", path.display())),
            Err(error) => ProbeResult::Err(format!("{error:#}")),
        }
    }

    fn legacy_spec_findings(&self) -> Vec<Finding> {
        let scan = match self.legacy_filesystems.scan() {
            Ok(scan) => scan,
            Err(error) => {
                return vec![Finding::from_probe(
                    Section::Profile,
                    Check::LegacyFilesystemSpec,
                    None,
                    ProbeResult::Err(format!("{error:#}")),
                )];
            },
        };
        let mut findings = scan
            .issues
            .into_iter()
            .map(|issue| Finding {
                section: Section::Profile,
                check: Check::LegacyFilesystemSpec,
                target: Some(issue.path.display().to_string()),
                severity: Severity::Attention,
                message: issue.message,
                fix: None,
                remediation: None,
            })
            .collect::<Vec<_>>();
        findings.extend(scan.specs.into_iter().map(|spec| Finding {
            section: Section::Profile,
            check: Check::LegacyFilesystemSpec,
            target: Some(spec.id().to_string()),
            severity: Severity::Attention,
            message:
                "legacy detached spec is not desired state and will not be launched".to_owned(),
            fix: Some(format!(
                "omnifs fs create --name {} --protocol {} --runtime {}",
                spec.id(),
                spec.protocol(),
                spec.runtime()
            )),
            remediation: None,
        }));
        findings
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
    use tempfile::TempDir;

    fn probes() -> Vec<Finding> {
        vec![
            Finding::from_probe(
                Section::Environment,
                Check::Docker,
                None,
                ProbeResult::Ok("docker daemon responds".to_string()),
            ),
            Finding::from_probe(
                Section::Environment,
                Check::Fuse,
                None,
                ProbeResult::Skipped("macOS: native mount is NFS loopback"),
            ),
            Finding::from_probe(
                Section::Environment,
                Check::Network,
                None,
                ProbeResult::Ok("ghcr.io reachable".to_string()),
            ),
            Finding::from_probe(
                Section::Profile,
                Check::Config,
                None,
                ProbeResult::Ok("defaults (~/.omnifs/config.toml absent)".to_string()),
            ),
        ]
    }

    fn targeted_finding() -> Finding {
        Finding {
            section: Section::Mounts,
            check: Check::Credentials,
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
        // Groups are separated by a blank line, not
        // run together.
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

        let verdict = lines.last().copied().unwrap_or_default();
        assert_eq!(verdict, "1 warning. Fix it:  omnifs mount reauth github");
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
            inventory: Inventory::test(
                crate::inventory::DaemonHealth::Stopped,
                Vec::new(),
                Vec::new(),
            ),
            findings: Vec::new(),
            repairs: Vec::new(),
        };
        assert_eq!(clean.verdict(), DoctorVerdict::Clean);

        let degraded = DoctorResult {
            inventory: Inventory::test(
                crate::inventory::DaemonHealth::Failed,
                Vec::new(),
                Vec::new(),
            ),
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
            inventory: Inventory::test(
                crate::inventory::DaemonHealth::Stopped,
                Vec::new(),
                Vec::new(),
            ),
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
        // `section` and `remediation` are presentation/execution-only and
        // must not grow the machine contract.
        assert!(value["findings"][0].get("section").is_none());
        assert!(value["findings"][0].get("remediation").is_none());
        // Empty repairs never appear in the payload; only an attempted run
        // grows the machine contract with a `repairs` array.
        assert!(value.get("repairs").is_none());
    }

    #[test]
    fn doctor_json_includes_repairs_when_present() {
        let payload = DoctorResult {
            inventory: Inventory::test(
                crate::inventory::DaemonHealth::Stopped,
                Vec::new(),
                Vec::new(),
            ),
            findings: Vec::new(),
            repairs: vec![
                Repair::applied("omnifs fs detach --name docker".to_owned()),
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

        assert!(remediable_fixes(&probes()).is_empty());
    }

    fn probe_credential_result(state: crate::inventory::DaemonHealth) -> ProbeResult {
        let root = TempDir::new().unwrap();
        let legacy_filesystems = LegacyFilesystems::under_profile(root.path());
        let profile = Profile::under_root(root.path());
        let daemon_runtime_paths = daemon_runtime_paths(&profile).unwrap();
        let doctor = Doctor {
            profile,
            legacy_filesystems,
            daemon_runtime_paths,
            inventory: Inventory::test(state, Vec::new(), Vec::new()),
            docker_target: Err("test".to_owned()),
            output: Output::new(crate::ui::output::OutputMode::Human, false),
        };
        doctor.probe_credential_store()
    }

    #[test]
    fn credential_probe_uses_daemon_inventory_only() {
        let result = probe_credential_result(crate::inventory::DaemonHealth::Stopped);
        assert!(
            matches!(result, ProbeResult::Warn(message) if message.contains("inventory unavailable"))
        );

        let result = probe_credential_result(crate::inventory::DaemonHealth::Running);
        assert!(
            matches!(result, ProbeResult::Ok(message) if message.contains("0 credentials") && message.contains("managed by daemon"))
        );
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
    async fn stopped_daemon_guard_excludes_start_for_its_full_lifetime() {
        let root = TempDir::new().unwrap();
        let profile = Profile::under_root(root.path());
        let guard = acquire_stopped_daemon_guard(&profile).await.unwrap();
        let contender = profile.clone();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let _lock = contender.acquire_spawn_lock().unwrap();
            acquired_tx.send(()).unwrap();
        });

        started_rx.recv().unwrap();
        assert!(
            acquired_rx
                .recv_timeout(std::time::Duration::from_millis(150))
                .is_err(),
            "a daemon start acquired the spawn lock during Doctor repair"
        );
        drop(guard);
        acquired_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        thread.join().unwrap();
    }

    #[tokio::test]
    async fn stale_host_repair_never_touches_a_replacement_record() {
        let root = TempDir::new().unwrap();
        let profile = Profile::under_root(root.path());
        let paths = daemon_runtime_paths(&profile).unwrap();
        let id = fs::Id::new("legacy").unwrap();
        let spec = fs::Spec::new(
            id.clone(),
            fs::Protocol::Nfs,
            fs::Runtime::Host,
            root.path().join("mount"),
        )
        .unwrap();
        let state_dir = paths.attachment(&id).state_dir().to_path_buf();
        std::fs::create_dir_all(&state_dir).unwrap();
        let record = |instance_id: &str| omnifs_mtab::RunnerRecord {
            version: omnifs_mtab::RunnerRecord::VERSION,
            instance_id: instance_id.to_owned(),
            pid: 1,
            process_group: 1,
            spec: spec.clone(),
            control_socket: state_dir.join(format!("{instance_id}.sock")),
        };
        let expected = record("11111111111111111111111111111111");
        let replacement = record("22222222222222222222222222222222");
        std::fs::write(
            state_dir.join("runner.json"),
            serde_json::to_vec(&replacement).unwrap(),
        )
        .unwrap();

        let error = Remediation::CleanStaleHostRecord {
            paths,
            state_dir: state_dir.clone(),
            record: expected,
        }
        .apply(&profile)
        .await
        .unwrap_err();
        assert!(error.to_string().contains("identity changed"), "{error:#}");
        assert_eq!(
            omnifs_mtab::RunnerRecord::read(&state_dir)
                .unwrap()
                .unwrap(),
            replacement
        );
    }
}
