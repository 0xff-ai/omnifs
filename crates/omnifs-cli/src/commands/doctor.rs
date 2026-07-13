//! `omnifs doctor` — environment + auth diagnostics. No auto-fix.

use clap::Args;
use serde::Serialize;
use std::path::Path;

use omnifs_workspace::creds::{CredentialStore, FileStore};

use crate::frontend_container::{frontend_container_name, resolve_frontend_image};
use crate::inventory::{Inventory, Severity};
use crate::launch_backend::{DockerTarget, ImageRef, names_registry};
use crate::runtime::Runtime;
use crate::ui::output::{Output, ResultVerdict};
use crate::ui::table::{
    Action as TableAction, Block as TableBlock, Cell as TableCell, Column as TableColumn,
    ContextStrip as TableContext, Priority as TablePriority, Report as TableReport,
    ResourceRow as TableRow, ResourceTable as TableResources, StateToken as TableState,
    WidthPolicy as TableWidth,
};
use crate::workspace::Workspace;
use omnifs_workspace::layout::WorkspaceLayout;
use omnifs_workspace::provider::DirStatus;

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
    fn label(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Warnings => "warnings",
            Self::Failures => "failures",
        }
    }
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
    human_probes: Vec<HumanProbe>,
    output: Output,
}

struct HumanProbe {
    name: String,
    result: ProbeResult,
}

impl DoctorReport {
    fn new(output: Output) -> Self {
        Self {
            verdict: DoctorVerdict::Clean,
            probes: Vec::new(),
            human_probes: Vec::new(),
            output,
        }
    }

    fn record(&mut self, name: impl Into<String>, result: ProbeResult) {
        let name = name.into();
        let (state, message) = match &result {
            ProbeResult::Ok(message) => ("ok", message.clone()),
            ProbeResult::Warn(message) => {
                if self.verdict == DoctorVerdict::Clean {
                    self.verdict = DoctorVerdict::Warnings;
                }
                ("warn", message.clone())
            },
            ProbeResult::Err(message) => {
                self.verdict = DoctorVerdict::Failures;
                ("err", message.clone())
            },
            ProbeResult::Skipped(reason) => ("skipped", (*reason).to_owned()),
        };
        self.human_probes.push(HumanProbe {
            name: name.clone(),
            result,
        });
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

    fn finish(self, live: LiveSection, paths: &WorkspaceLayout) -> anyhow::Result<DoctorVerdict> {
        if self.output.is_structured() {
            self.output.emit_result(
                match self.verdict {
                    DoctorVerdict::Clean => ResultVerdict::Ok,
                    DoctorVerdict::Warnings | DoctorVerdict::Failures => ResultVerdict::Degraded,
                },
                DoctorJson {
                    verdict: self.verdict.label(),
                    probes: self.probes,
                    live,
                },
            )?;
        } else {
            let failures = self.count("err") + live_count(&live, "err");
            let warnings = self.count("warn") + live_count(&live, "warn");
            let mut report = TableReport::new();
            report.push(TableBlock::Resources(diagnostics_table(&self.human_probes)));
            report.push(TableBlock::Resources(live_table(&live)));
            report.push(TableBlock::Resources(paths_table(paths)));
            let state = match self.verdict {
                DoctorVerdict::Clean => TableState::positive("clean"),
                DoctorVerdict::Warnings => TableState::attention("warnings"),
                DoctorVerdict::Failures => TableState::failure("failures"),
            };
            report.push(TableBlock::Context(TableContext::new(
                "Verdict",
                verdict_line(self.verdict, warnings, failures),
                state,
            )));
            report.print();
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

fn diagnostics_table(probes: &[HumanProbe]) -> TableResources {
    let mut table = TableResources::new(
        "Diagnostics",
        probes.len(),
        vec![
            TableColumn::new("Check", TablePriority::Identity, TableWidth::Auto),
            TableColumn::new("Details", TablePriority::Essential, TableWidth::Auto),
            TableColumn::new("State", TablePriority::Essential, TableWidth::Auto),
        ],
    );
    for probe in probes {
        let state = probe.result.state_token();
        table.push(TableRow::new(
            [
                TableCell::new(probe.name.clone()),
                TableCell::new(probe.result.message()),
                TableCell::state(state.clone()),
            ],
            state,
        ));
    }
    table
}

fn live_table(live: &LiveSection) -> TableResources {
    let count = live.findings.len().max(1);
    let mut table = TableResources::new(
        "Live daemon",
        count,
        vec![
            TableColumn::new("Mount", TablePriority::Identity, TableWidth::Auto),
            TableColumn::new("Details", TablePriority::Essential, TableWidth::Auto),
            TableColumn::new("State", TablePriority::Essential, TableWidth::Auto),
        ],
    );
    if let Some(reason) = &live.skipped {
        let state = TableState::neutral("skipped");
        table.push(TableRow::new(
            [
                TableCell::new("daemon"),
                TableCell::new(reason.clone()),
                TableCell::state(state.clone()),
            ],
            state,
        ));
    } else if live.findings.is_empty() {
        let state = TableState::positive("healthy");
        table.push(TableRow::new(
            [
                TableCell::new("all mounts"),
                TableCell::new("all live mounts are healthy"),
                TableCell::state(state.clone()),
            ],
            state,
        ));
    } else {
        for finding in &live.findings {
            let state = if finding.state == "err" {
                TableState::failure("err")
            } else {
                TableState::attention("warn")
            };
            table.push(
                TableRow::new(
                    [
                        TableCell::new(finding.mount.clone()),
                        TableCell::new(finding.message.clone()),
                        TableCell::state(state.clone()),
                    ],
                    state,
                )
                .with_action(TableAction::fix(finding.fix.clone())),
            );
        }
    }
    table
}

fn paths_table(layout: &WorkspaceLayout) -> TableResources {
    let paths = [
        ("config", &layout.config_dir),
        ("cache", &layout.cache_dir),
        ("mounts", &layout.mounts_dir),
        ("providers", &layout.providers_dir),
        ("credentials", &layout.credentials_file),
        ("config file", &layout.config_file),
    ];
    let mut table = TableResources::new(
        "Paths",
        paths.len(),
        vec![
            TableColumn::new("Path", TablePriority::Identity, TableWidth::Auto),
            TableColumn::new("Location", TablePriority::Essential, TableWidth::Path),
            TableColumn::new("State", TablePriority::Essential, TableWidth::Auto),
        ],
    );
    for (key, path) in paths {
        let state = TableState::neutral("configured");
        table.push(TableRow::new(
            [
                TableCell::new(key),
                TableCell::new(WorkspaceLayout::display(path)),
                TableCell::state(state.clone()),
            ],
            state,
        ));
    }
    table
}

#[derive(Debug)]
enum ProbeResult {
    Ok(String),
    Warn(String),
    Err(String),
    Skipped(&'static str),
}

impl ProbeResult {
    fn message(&self) -> &str {
        match self {
            Self::Ok(m) | Self::Warn(m) | Self::Err(m) => m.as_str(),
            Self::Skipped(reason) => reason,
        }
    }

    fn state_token(&self) -> TableState {
        match self {
            Self::Ok(_) => TableState::positive("ok"),
            Self::Warn(_) => TableState::attention("warn"),
            Self::Err(_) => TableState::failure("err"),
            Self::Skipped(_) => TableState::neutral("skipped"),
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

        let live = self.probe_live();
        report.record_live(&live);
        report.finish(live, self.workspace.layout())
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

    fn probe_providers_discovered(&self) -> ProbeResult {
        match self.workspace.catalog().dir_status() {
            DirStatus::Present { wasm_count } if wasm_count > 0 => {
                match self.workspace.catalog().installable() {
                    Ok(providers) => ProbeResult::Ok(format!(
                        "{} providers ({wasm_count} artifacts)",
                        providers.len()
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

    fn probe_mount_configs(&self) -> (ProbeResult, Vec<(String, ProbeResult)>) {
        let invalid = self
            .inventory
            .mounts
            .iter()
            .filter(|mount| mount.auth.severity() == Severity::Error)
            .count();
        let valid = self.inventory.mounts.len().saturating_sub(invalid);
        let configs = if invalid == 0 {
            ProbeResult::Ok(format!("{valid} mount(s) valid"))
        } else {
            ProbeResult::Err(format!("{valid} valid, {invalid} invalid"))
        };
        let auth = self
            .inventory
            .mounts
            .iter()
            .map(|mount| {
                let result = match mount.auth.severity() {
                    Severity::Positive | Severity::Neutral => {
                        ProbeResult::Ok(mount.auth.label().to_owned())
                    },
                    Severity::Attention => ProbeResult::Warn(mount.auth.label().to_owned()),
                    Severity::Error => ProbeResult::Err(mount.auth.label().to_owned()),
                };
                (mount.name.clone(), result)
            })
            .collect();
        (configs, auth)
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

    fn probe_live(&self) -> LiveSection {
        if self.inventory.workspace.daemon == crate::inventory::DaemonState::Stopped {
            return LiveSection {
                skipped: Some("daemon is stopped".to_string()),
                findings: Vec::new(),
            };
        }
        let findings = self
            .inventory
            .mounts
            .iter()
            .filter_map(|mount| {
                let state = if mount.serving.severity() == Severity::Error {
                    "err"
                } else if mount.auth.severity() >= Severity::Attention {
                    "warn"
                } else {
                    return None;
                };
                Some(LiveFinding {
                    mount: mount.name.clone(),
                    state,
                    message: format!(
                        "auth={} serving={}",
                        mount.auth.label(),
                        mount.serving.label()
                    ),
                    fix: mount.fix.clone().unwrap_or_else(|| "omnifs logs".into()),
                })
            })
            .collect();
        LiveSection {
            skipped: None,
            findings,
        }
    }
}

#[cfg(test)]
mod golden {
    use super::*;
    use crate::test_support::fixture_paths;
    use crate::ui::strip_ansi;
    use tempfile::TempDir;

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
        let human = probes
            .into_iter()
            .map(|(name, result)| HumanProbe {
                name: name.to_owned(),
                result,
            })
            .collect::<Vec<_>>();
        let live = live();
        let mut report = TableReport::new();
        report.push(TableBlock::Resources(diagnostics_table(&human)));
        report.push(TableBlock::Resources(live_table(&live)));
        let rendered = strip_ansi(&report.render());
        assert!(rendered.contains("Diagnostics  5"));
        assert!(rendered.contains("Live daemon  1"));
        assert!(rendered.contains("Fix  omnifs mount reauth linear"));
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
        let mut report =
            DoctorReport::new(Output::new(crate::ui::output::OutputMode::Human, false));
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

    fn probe_credential_result(root: &std::path::Path) -> ProbeResult {
        let layout = fixture_paths(root);
        let workspace = Workspace::from_layout(layout.clone());
        let doctor = Doctor {
            workspace: &workspace,
            inventory: Inventory {
                workspace: crate::inventory::WorkspaceStatus {
                    home: layout.config_dir.clone(),
                    daemon: crate::inventory::DaemonState::Stopped,
                    namespace: crate::inventory::NamespaceState::Offline,
                    pid: None,
                    api: None,
                    runtime_expected: false,
                },
                frontends: Vec::new(),
                mounts: Vec::new(),
                providers: Vec::new(),
            },
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
            r#"{"version":1,"entries":{"not-a-credential-key":{"kind":"static-token","access_token":"x","stored_at":"1970-01-01T00:00:00Z"}}}"#,
        )
        .unwrap();
        let result = probe_credential_result(bad_key.path());
        assert!(
            matches!(result, ProbeResult::Err(message) if message.contains("credential") && message.contains("key"))
        );
    }
}
