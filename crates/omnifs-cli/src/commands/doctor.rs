//! `omnifs doctor` — runtime + auth diagnostics. No auto-fix.

use clap::Args;
use serde::Serialize;
use std::path::Path;

use omnifs_workspace::creds::{CredentialStore, FileStore};

use crate::docker::DockerClient;
use crate::docker::DockerTarget;
use crate::frontend_container::{frontend_container_name, resolve_frontend_image};
use crate::image::{ImageRef, names_registry};
use crate::inventory::{Inventory, Severity};
use crate::status::InventoryReport;
use crate::ui::output::{Output, ResultVerdict};
use crate::ui::table::{
    Action as TableAction, Block as TableBlock, Cell as TableCell, Column as TableColumn,
    Priority as TablePriority, ResourceRow as TableRow, ResourceTable as TableResources,
    StateToken as TableState, WidthPolicy as TableWidth,
};
use crate::workspace::Workspace;
use omnifs_workspace::layout::WorkspaceLayout;

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
    inventory: Inventory,
    /// The frontend's Docker target, or the error resolving it.
    docker_target: Result<DockerTarget, String>,
    output: Output,
}

#[derive(Debug, Clone, Serialize)]
struct Finding {
    check: String,
    target: Option<String>,
    severity: Severity,
    message: String,
    fix: Option<String>,
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
    fn from_probe(check: impl Into<String>, target: Option<String>, result: ProbeResult) -> Self {
        let (severity, message) = result.into_parts();
        Self {
            check: check.into(),
            target,
            severity,
            message,
            fix: None,
        }
    }
}

fn findings_table(findings: &[Finding]) -> TableResources {
    let mut table = TableResources::new(
        "Findings",
        findings.len().max(1),
        vec![
            TableColumn::new("Check", TablePriority::Identity, TableWidth::Auto),
            TableColumn::new("Target", TablePriority::Secondary, TableWidth::Auto),
            TableColumn::new("Details", TablePriority::Essential, TableWidth::Auto),
            TableColumn::new("Severity", TablePriority::Essential, TableWidth::Auto),
        ],
    );
    if findings.is_empty() {
        let state = TableState::positive("clean");
        table.push(TableRow::new(
            [
                TableCell::new("all checks"),
                TableCell::new("-"),
                TableCell::new("no findings"),
                TableCell::state(state.clone()),
            ],
            state,
        ));
    } else {
        for finding in findings {
            let state = table_state(finding.severity);
            let mut row = TableRow::new(
                [
                    TableCell::new(finding.check.clone()),
                    TableCell::new(finding.target.as_deref().unwrap_or("-")),
                    TableCell::new(finding.message.clone()),
                    TableCell::state(state.clone()),
                ],
                state,
            );
            if let Some(fix) = &finding.fix {
                row = row.with_action(TableAction::fix(fix.clone()));
            }
            table.push(row);
        }
    }
    table
}

fn table_state(severity: Severity) -> TableState {
    match severity {
        Severity::Positive => TableState::positive("ok"),
        Severity::Neutral => TableState::neutral("skipped"),
        Severity::Attention => TableState::attention("warn"),
        Severity::Error => TableState::failure("err"),
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

impl Doctor<'_> {
    async fn run(self) -> anyhow::Result<DoctorVerdict> {
        let mut findings = Vec::new();

        let (runtime, docker_result) = self.probe_docker_reachable().await;
        let docker_ok = matches!(docker_result, ProbeResult::Ok(_));
        findings.push(Finding::from_probe("docker reachable", None, docker_result));

        findings.push(Finding::from_probe("fuse", None, self.probe_fuse()));

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
        findings.push(Finding::from_probe("image cached", None, image_result));

        findings.push(Finding::from_probe(
            "credential store",
            None,
            self.probe_credential_store(),
        ));
        findings.push(Finding::from_probe(
            "ssh-agent",
            None,
            self.probe_ssh_agent(),
        ));
        findings.push(Finding::from_probe(
            "config file",
            None,
            self.probe_config_file(),
        ));

        findings.push(Finding::from_probe(
            "network",
            None,
            self.probe_network().await,
        ));

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
            let mut report = InventoryReport {
                inventory: result.inventory,
            }
            .render();
            report.push(TableBlock::Resources(findings_table(&result.findings)));
            report.print();
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
        let credentials_file = &self.workspace.layout().credentials_file;
        let Some(parent) = credentials_file.parent() else {
            return ProbeResult::Err(format!(
                "credential file has no parent: {}",
                credentials_file.display()
            ));
        };
        if !parent.exists() {
            ProbeResult::Warn(format!(
                "credential directory will be created on first write: {}",
                WorkspaceLayout::display(parent)
            ))
        } else if !credentials_file.exists() {
            ProbeResult::Warn(format!(
                "credential store not created yet: {}",
                WorkspaceLayout::display(credentials_file)
            ))
        } else {
            match FileStore::new(credentials_file).list() {
                Ok(Some(keys)) => ProbeResult::Ok(format!(
                    "{} credential(s) in {}",
                    keys.len(),
                    WorkspaceLayout::display(credentials_file)
                )),
                Ok(None) => ProbeResult::Ok(format!(
                    "credential store available at {}",
                    WorkspaceLayout::display(credentials_file)
                )),
                Err(error) => ProbeResult::Err(format!(
                    "credential store {} unreadable: {error}",
                    WorkspaceLayout::display(credentials_file)
                )),
            }
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
    use crate::test_support::fixture_paths;
    use crate::ui::strip_ansi;
    use crate::ui::table::Report as TableReport;
    use tempfile::TempDir;

    fn probes() -> Vec<Finding> {
        vec![
            Finding::from_probe(
                "docker reachable",
                None,
                ProbeResult::Ok("docker daemon responds".to_string()),
            ),
            Finding::from_probe(
                "fuse",
                None,
                ProbeResult::Skipped("macOS: native mount is NFS loopback"),
            ),
            Finding::from_probe(
                "config identity",
                None,
                ProbeResult::Ok("workspace paths agree".to_string()),
            ),
            Finding::from_probe(
                "credential store",
                None,
                ProbeResult::Warn("directory will be created on first write".to_string()),
            ),
            Finding::from_probe(
                "network",
                None,
                ProbeResult::Err("ghcr.io unreachable".to_string()),
            ),
        ]
    }

    fn targeted_finding() -> Finding {
        Finding {
            check: "credential target".to_string(),
            target: Some("linear".to_string()),
            severity: Severity::Attention,
            message: "credential `linear:oauth:default` is expired".to_string(),
            fix: Some("omnifs mount reauth linear".to_string()),
        }
    }

    #[test]
    fn doctor_grid() {
        let probes = probes();
        let mut findings = probes;
        findings.push(targeted_finding());
        let mut report = TableReport::new();
        report.push(TableBlock::Resources(findings_table(&findings)));
        let rendered = strip_ansi(&report.render());
        assert!(rendered.contains("Findings  6"));
        assert!(rendered.contains("Fix  omnifs mount reauth linear"));
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
            check: "broken".to_owned(),
            target: None,
            severity: Severity::Error,
            message: "failed to load".to_owned(),
            fix: Some("omnifs logs".to_owned()),
        });
        assert_eq!(failures.verdict(), DoctorVerdict::Failures);
    }

    #[test]
    fn doctor_json_preserves_inventory_and_findings() {
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
        assert_eq!(value["findings"][0]["check"], "credential target");
        assert_eq!(value["findings"][0]["target"], "linear");
        assert_eq!(value["findings"][0]["severity"], "attention");
        assert_eq!(value["findings"][0]["fix"], "omnifs mount reauth linear");
    }

    fn probe_credential_result(root: &std::path::Path) -> ProbeResult {
        let layout = fixture_paths(root);
        let workspace = Workspace::from_layout(layout.clone());
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
        let valid_path = fixture_paths(valid.path()).credentials_file;
        std::fs::create_dir_all(valid.path()).unwrap();
        std::fs::write(&valid_path, r#"{"version":1,"entries":{}}"#).unwrap();
        let result = probe_credential_result(valid.path());
        assert!(
            matches!(result, ProbeResult::Ok(message) if message.contains("0 credential(s)") && message.contains("credentials.json"))
        );

        let invalid = TempDir::new().unwrap();
        let invalid_path = fixture_paths(invalid.path()).credentials_file;
        std::fs::write(&invalid_path, "not json").unwrap();
        let result = probe_credential_result(invalid.path());
        assert!(
            matches!(result, ProbeResult::Err(message) if message.contains("credentials.json") && message.contains("unreadable"))
        );

        let unsupported = TempDir::new().unwrap();
        let unsupported_path = fixture_paths(unsupported.path()).credentials_file;
        std::fs::write(&unsupported_path, r#"{"version":99,"entries":{}}"#).unwrap();
        let result = probe_credential_result(unsupported.path());
        assert!(
            matches!(result, ProbeResult::Err(message) if message.contains("version") && message.contains("99"))
        );

        let bad_key = TempDir::new().unwrap();
        let bad_key_path = fixture_paths(bad_key.path()).credentials_file;
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
