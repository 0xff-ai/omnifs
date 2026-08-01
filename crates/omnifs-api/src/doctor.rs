//! Typed doctor findings and repair results returned by the daemon.

use serde::{Deserialize, Serialize};

/// Group of checks that produced a doctor finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorSection {
    Environment,
    Profile,
    Mounts,
    Filesystems,
}

/// Specific check represented by a doctor finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorCheckKind {
    Docker,
    Fuse,
    Image,
    Network,
    SshAgent,
    Config,
    CredentialStore,
    Credentials,
    FilesystemState,
    StrayFilesystem,
    StaleFilesystemState,
    DockerFilesystemOwnership,
    LibkrunFilesystemOwnership,
}

/// Severity used when folding a complete doctor report into a verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorSeverity {
    Positive,
    Neutral,
    Attention,
    Failure,
}

/// One daemon-produced diagnostic finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct DoctorFinding {
    pub section: DoctorSection,
    pub check: DoctorCheckKind,
    pub target: Option<String>,
    pub severity: DoctorSeverity,
    pub message: String,
    pub fix: Option<String>,
    /// Opaque daemon-side remediation id. It is absent for report-only rows.
    pub remediation_id: Option<String>,
}

/// Executor for one daemon-offered remediation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorExecutor {
    Daemon,
    ClientMountReauth { mount: String },
}

/// One remediation offered alongside a doctor report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct DoctorRemediation {
    pub id: String,
    pub command_line: String,
    pub executor: DoctorExecutor,
}

/// Complete doctor response, including findings and executable remediations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct RunDoctorReport {
    pub findings: Vec<DoctorFinding>,
    pub remediations: Vec<DoctorRemediation>,
}

/// State of one attempted remediation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorRepairState {
    Applied,
    Failed,
    Skipped,
}

/// Outcome for one requested remediation id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct DoctorRepairOutcome {
    pub id: String,
    pub command_line: String,
    pub state: DoctorRepairState,
    pub error: Option<String>,
}
