//! Daemon-owned diagnostics and exact runtime repair offers.
//!
//! The daemon is the only process that can see its complete runtime state and
//! SQLite desired state at once.  This module produces the full doctor report
//! and keeps short-lived, opaque repair offers so a client can request an
//! exact repair without sending runtime details back over RPC.

use anyhow::{Context as _, Result};
use omnifs_api::{
    CredentialHealth, DoctorCheckKind, DoctorExecutor, DoctorFinding, DoctorRemediation,
    DoctorRepairOutcome, DoctorRepairState, DoctorSection, DoctorSeverity, HealthState,
    MountHealth, RunDoctorReport,
};
use omnifs_core::ResourceName;
use omnifs_state::StateStore;
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::daemon::Daemon;
use crate::fs_runtime::{
    Candidate, DockerClient, DockerContainerIdentity, DockerTarget, HostDriver, ImageInspection,
    LibkrunRunner, OwnedFilesystemContainer, RuntimeEventSink, RuntimePaths, owned_filesystems,
};

const DOCKER_PING_TIMEOUT: Duration = Duration::from_secs(3);
const NETWORK_TIMEOUT: Duration = Duration::from_secs(3);
const OFFER_TTL: Duration = Duration::from_secs(5 * 60);
const OFFER_LIMIT: usize = 256;
const UNKNOWN_REMEDIATION: &str = "remediation id unknown or no longer offered";
const OWNERSHIP_SCAN_TIMEOUT: Duration = Duration::from_secs(3);
const BASE_FINDING_ORDER: [DoctorCheckKind; 7] = [
    DoctorCheckKind::Docker,
    DoctorCheckKind::Fuse,
    DoctorCheckKind::Image,
    DoctorCheckKind::CredentialStore,
    DoctorCheckKind::SshAgent,
    DoctorCheckKind::Config,
    DoctorCheckKind::Network,
];

/// Daemon-owned short-lived remediation state.  The lock is held only while
/// minting, pruning, or claiming map entries.  Runtime, network, and state
/// operations always happen after the lock is released.
pub(crate) struct DoctorState {
    offers: tokio::sync::Mutex<HashMap<String, StoredOffer>>,
}

impl DoctorState {
    pub(crate) fn new() -> Self {
        Self {
            offers: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    async fn mint(
        &self,
        command_line: String,
        executor: DoctorExecutor,
        repair: DoctorRepair,
    ) -> Result<DoctorRemediation> {
        let mut offers = self.offers.lock().await;
        prune_offers(&mut offers);
        let id = loop {
            let mut id_bytes = [0_u8; 16];
            getrandom::fill(&mut id_bytes).context("generate doctor remediation id")?;
            let id = hex::encode(id_bytes);
            if !offers.contains_key(&id) {
                break id;
            }
        };
        while offers.len() >= OFFER_LIMIT {
            let oldest = offers
                .iter()
                .min_by_key(|(_, offer)| offer.expires_at)
                .map(|(id, _)| id.clone());
            let Some(oldest) = oldest else { break };
            offers.remove(&oldest);
        }
        offers.insert(
            id.clone(),
            StoredOffer {
                command_line: command_line.clone(),
                executor: executor.clone(),
                repair,
                expires_at: Instant::now() + OFFER_TTL,
            },
        );
        Ok(DoctorRemediation {
            id,
            command_line,
            executor,
        })
    }

    async fn claim(&self, ids: &[String]) -> Vec<(String, Option<StoredOffer>)> {
        let mut offers = self.offers.lock().await;
        prune_offers(&mut offers);
        let mut seen = std::collections::HashSet::new();
        ids.iter()
            .filter(|id| seen.insert((*id).clone()))
            .map(|id| (id.clone(), offers.remove(id)))
            .collect()
    }
}

fn prune_offers(offers: &mut HashMap<String, StoredOffer>) {
    let now = Instant::now();
    offers.retain(|_, offer| offer.expires_at > now);
}

struct StoredOffer {
    command_line: String,
    executor: DoctorExecutor,
    repair: DoctorRepair,
    expires_at: Instant,
}

enum DoctorRepair {
    ClientMountReauth,
    StopHost {
        paths: RuntimePaths,
        state_dir: PathBuf,
        record: omnifs_mtab::RunnerRecord,
    },
    CleanupHost {
        paths: RuntimePaths,
        state_dir: PathBuf,
        record: omnifs_mtab::RunnerRecord,
    },
    StopLibkrun {
        state_dir: PathBuf,
        record: omnifs_libkrun::HelperRecord,
    },
}

impl Daemon {
    /// Run all daemon-owned doctor probes and mint matching remediation
    /// offers.  Every offer carries only its opaque id over the wire; runtime
    /// records remain in this daemon-side map.
    pub(crate) async fn run_doctor(&self) -> Result<RunDoctorReport> {
        let paths = self.runtime_paths();
        let mut findings = Vec::new();
        let mut remediations = Vec::new();

        let (docker, docker_finding) = self.probe_docker().await;
        let fuse_finding = probe_fuse();
        let image_finding = self.probe_image(docker.as_ref()).await;
        let credential_store_finding = self.probe_credential_store().await?;
        let ssh_agent_finding = probe_ssh_agent();
        let config_finding = self.probe_config();
        let network_finding = self.probe_network().await;
        for check in BASE_FINDING_ORDER {
            findings.push(match check {
                DoctorCheckKind::Docker => docker_finding.clone(),
                DoctorCheckKind::Fuse => fuse_finding.clone(),
                DoctorCheckKind::Image => image_finding.clone(),
                DoctorCheckKind::CredentialStore => credential_store_finding.clone(),
                DoctorCheckKind::SshAgent => ssh_agent_finding.clone(),
                DoctorCheckKind::Config => config_finding.clone(),
                DoctorCheckKind::Network => network_finding.clone(),
                _ => unreachable!("base doctor finding order contains a non-base check"),
            });
        }

        for mount in self.mount_records().await? {
            let Some((severity, message)) = mount_auth_finding(&mount) else {
                continue;
            };
            let name = mount.definition.name.to_string();
            let command_line = format!("omnifs mount reauth {name}");
            let remediation = self
                .doctor
                .mint(
                    command_line.clone(),
                    DoctorExecutor::ClientMountReauth {
                        mount: name.clone(),
                    },
                    DoctorRepair::ClientMountReauth,
                )
                .await?;
            findings.push(DoctorFinding {
                section: DoctorSection::Mounts,
                check: DoctorCheckKind::Credentials,
                target: Some(name),
                severity,
                message,
                fix: Some(command_line),
                remediation_id: Some(remediation.id.clone()),
            });
            remediations.push(remediation);
        }

        let live = self.live_filesystems();
        let mut filesystem_findings = self
            .filesystem_findings(&paths, docker.as_ref(), &live)
            .await?;
        remediations.extend(filesystem_findings.1.drain(..));
        findings.extend(filesystem_findings.0);

        Ok(RunDoctorReport {
            findings,
            remediations,
        })
    }

    /// Claim each requested id once, then execute the claimed runtime repair
    /// outside the offer-map lock.  A fresh desired/observed state read gates
    /// every filesystem effect.
    pub(crate) async fn apply_doctor_repairs(
        &self,
        ids: Vec<String>,
    ) -> Result<Vec<DoctorRepairOutcome>> {
        let claimed = self.doctor.claim(&ids).await;
        let mut outcomes = Vec::with_capacity(claimed.len());
        for (id, offer) in claimed {
            let Some(offer) = offer else {
                outcomes.push(DoctorRepairOutcome {
                    id,
                    command_line: String::new(),
                    state: DoctorRepairState::Skipped,
                    error: Some(UNKNOWN_REMEDIATION.to_owned()),
                });
                continue;
            };
            let command_line = offer.command_line.clone();
            if matches!(offer.executor, DoctorExecutor::ClientMountReauth { .. }) {
                outcomes.push(DoctorRepairOutcome {
                    id,
                    command_line,
                    state: DoctorRepairState::Skipped,
                    error: Some("mount reauth remediation must run in the client".to_owned()),
                });
                continue;
            }
            let result = self.apply_runtime_repair(offer.repair).await;
            outcomes.push(match result {
                Ok(RepairResult::Applied) => DoctorRepairOutcome {
                    id,
                    command_line,
                    state: DoctorRepairState::Applied,
                    error: None,
                },
                Ok(RepairResult::Skipped(error)) => DoctorRepairOutcome {
                    id,
                    command_line,
                    state: DoctorRepairState::Skipped,
                    error: Some(error),
                },
                Err(error) => DoctorRepairOutcome {
                    id,
                    command_line,
                    state: DoctorRepairState::Failed,
                    error: Some(format!("{error:#}")),
                },
            });
        }
        Ok(outcomes)
    }

    async fn apply_runtime_repair(&self, repair: DoctorRepair) -> Result<RepairResult> {
        let name = match &repair {
            DoctorRepair::StopHost { record, .. } | DoctorRepair::CleanupHost { record, .. } => {
                record.filesystem.clone()
            },
            DoctorRepair::StopLibkrun { record, .. } => record.filesystem.clone(),
            DoctorRepair::ClientMountReauth => unreachable!("handled before runtime repair"),
        };
        let runtime_name = name.clone();
        apply_with_ownership_gate(&self.state, &name, || async move {
            match repair {
                DoctorRepair::StopHost {
                    paths,
                    state_dir,
                    record,
                } => {
                    let runtime = paths.filesystem(&runtime_name);
                    let driver = HostDriver::new(
                        state_dir,
                        runtime.host_log().to_path_buf(),
                        runtime.executable().to_path_buf(),
                        RuntimeEventSink::discard(),
                    );
                    driver.stop_confirmed(&record).await?;
                },
                DoctorRepair::CleanupHost {
                    paths,
                    state_dir,
                    record,
                } => {
                    let runtime = paths.filesystem(&runtime_name);
                    let driver = HostDriver::new(
                        state_dir,
                        runtime.host_log().to_path_buf(),
                        runtime.executable().to_path_buf(),
                        RuntimeEventSink::discard(),
                    );
                    driver.cleanup_stale(&record).await?;
                },
                DoctorRepair::StopLibkrun { state_dir, record } => {
                    let runner = LibkrunRunner::new(state_dir);
                    let Some((confirmed, _running)) =
                        runner.confirmed(&record.filesystem, &record.spec).await?
                    else {
                        return Ok(RepairResult::Applied);
                    };
                    anyhow::ensure!(
                        confirmed == record,
                        "libkrun helper identity changed before doctor repair"
                    );
                    runner.stop_confirmed(confirmed).await?;
                },
                DoctorRepair::ClientMountReauth => unreachable!("handled before runtime repair"),
            }
            Ok(RepairResult::Applied)
        })
        .await
    }

    fn runtime_paths(&self) -> RuntimePaths {
        RuntimePaths::from_daemon_state(
            self.context.profile().root().to_path_buf(),
            std::env::var_os(omnifs_bootstrap::OMNIFS_HOME_ENV).is_none(),
            self.context.state_paths(),
            self.context.process_identity().executable().to_path_buf(),
        )
    }

    async fn probe_docker(&self) -> (Option<DockerClient>, DoctorFinding) {
        let id = ResourceName::new("doctor").expect("static filesystem name");
        let target = match DockerTarget::for_filesystem(
            self.context.profile().root(),
            std::env::var_os(omnifs_bootstrap::OMNIFS_HOME_ENV).is_none(),
            &id,
            None,
        ) {
            Ok(target) => target,
            Err(error) => {
                return (
                    None,
                    finding(
                        DoctorSection::Environment,
                        DoctorCheckKind::Docker,
                        None,
                        Probe::Failure(format!("resolve target: {error:#}")),
                    ),
                );
            },
        };
        let runtime = match DockerClient::connect_for(&target, RuntimeEventSink::discard()) {
            Ok(runtime) => runtime,
            Err(error) => {
                return (
                    None,
                    finding(
                        DoctorSection::Environment,
                        DoctorCheckKind::Docker,
                        None,
                        Probe::Failure(format!("connect: {error}")),
                    ),
                );
            },
        };
        match tokio::time::timeout(DOCKER_PING_TIMEOUT, runtime.ping()).await {
            Ok(Ok(())) => (
                Some(runtime),
                finding(
                    DoctorSection::Environment,
                    DoctorCheckKind::Docker,
                    None,
                    Probe::Positive("docker daemon responds".to_owned()),
                ),
            ),
            Ok(Err(error)) => (
                None,
                finding(
                    DoctorSection::Environment,
                    DoctorCheckKind::Docker,
                    None,
                    Probe::Failure(format!("ping: {error}")),
                ),
            ),
            Err(_) => (
                None,
                finding(
                    DoctorSection::Environment,
                    DoctorCheckKind::Docker,
                    None,
                    Probe::Failure(format!(
                        "ping timed out after {}s",
                        DOCKER_PING_TIMEOUT.as_secs()
                    )),
                ),
            ),
        }
    }

    async fn probe_image(&self, runtime: Option<&DockerClient>) -> DoctorFinding {
        let Some(runtime) = runtime else {
            return finding(
                DoctorSection::Environment,
                DoctorCheckKind::Image,
                None,
                Probe::Neutral("docker unreachable".to_owned()),
            );
        };
        match runtime.inspect_image(runtime.image().as_str()).await {
            Ok(ImageInspection::Present) => finding(
                DoctorSection::Environment,
                DoctorCheckKind::Image,
                None,
                Probe::Positive(format!("{} cached", runtime.image())),
            ),
            Ok(ImageInspection::Missing) if runtime.image().has_registry() => finding(
                DoctorSection::Environment,
                DoctorCheckKind::Image,
                None,
                Probe::Attention(format!(
                    "{} not cached (will pull on the next Docker filesystem start)",
                    runtime.image()
                )),
            ),
            Ok(ImageInspection::Missing) => finding(
                DoctorSection::Environment,
                DoctorCheckKind::Image,
                None,
                Probe::Failure(format!(
                    "{} not present locally; a dev image is never pulled, so filesystem start cannot start (build it with `just filesystem-image`)",
                    runtime.image()
                )),
            ),
            Err(error) => finding(
                DoctorSection::Environment,
                DoctorCheckKind::Image,
                None,
                Probe::Failure(format!("inspect: {error}")),
            ),
        }
    }

    async fn probe_network(&self) -> DoctorFinding {
        let client = match reqwest::Client::builder().timeout(NETWORK_TIMEOUT).build() {
            Ok(client) => client,
            Err(error) => {
                return finding(
                    DoctorSection::Environment,
                    DoctorCheckKind::Network,
                    None,
                    Probe::Attention(format!("client build: {error}")),
                );
            },
        };
        let request = client.head("https://ghcr.io").send();
        match tokio::time::timeout(NETWORK_TIMEOUT, request).await {
            Ok(Ok(_)) => finding(
                DoctorSection::Environment,
                DoctorCheckKind::Network,
                None,
                Probe::Positive("ghcr.io reachable".to_owned()),
            ),
            Ok(Err(error)) => finding(
                DoctorSection::Environment,
                DoctorCheckKind::Network,
                None,
                Probe::Attention(format!("ghcr.io unreachable: {error}")),
            ),
            Err(_) => finding(
                DoctorSection::Environment,
                DoctorCheckKind::Network,
                None,
                Probe::Attention(format!(
                    "ghcr.io unreachable: request timed out after {}s",
                    NETWORK_TIMEOUT.as_secs()
                )),
            ),
        }
    }

    async fn probe_credential_store(&self) -> Result<DoctorFinding> {
        let count = self.state.list_credentials().await?.len();
        Ok(finding(
            DoctorSection::Profile,
            DoctorCheckKind::CredentialStore,
            None,
            Probe::Positive(format!("{} managed by daemon", credential_count(count))),
        ))
    }

    fn probe_config(&self) -> DoctorFinding {
        let path = self.context.profile().root().join("config.toml");
        match omnifs_bootstrap::profile_config::read(self.context.profile().root()) {
            Ok(_) if path.exists() => finding(
                DoctorSection::Profile,
                DoctorCheckKind::Config,
                None,
                Probe::Positive(path.display().to_string()),
            ),
            Ok(_) => finding(
                DoctorSection::Profile,
                DoctorCheckKind::Config,
                None,
                Probe::Positive(format!("defaults ({} absent)", path.display())),
            ),
            Err(error) => finding(
                DoctorSection::Profile,
                DoctorCheckKind::Config,
                None,
                Probe::Failure(format!("{error:#}")),
            ),
        }
    }

    async fn bounded_owned_filesystems(
        paths: &RuntimePaths,
        docker: Option<&DockerClient>,
    ) -> Vec<Candidate> {
        match tokio::time::timeout(OWNERSHIP_SCAN_TIMEOUT, owned_filesystems(paths, docker)).await {
            Ok(candidates) => candidates,
            Err(_) => vec![Candidate::ListingFailed {
                backend: "host",
                error: format!(
                    "filesystem ownership scan timed out after {}s",
                    OWNERSHIP_SCAN_TIMEOUT.as_secs()
                ),
            }],
        }
    }

    async fn filesystem_findings(
        &self,
        paths: &RuntimePaths,
        docker: Option<&DockerClient>,
        live: &[omnifs_api::FilesystemDefinition],
    ) -> Result<(Vec<DoctorFinding>, Vec<DoctorRemediation>)> {
        let candidates = Self::bounded_owned_filesystems(paths, docker).await;
        let mut findings = Vec::new();
        let mut remediations = Vec::new();
        for candidate in candidates {
            match candidate {
                Candidate::ListingFailed { backend, error } => findings.push(finding(
                    DoctorSection::Filesystems,
                    ownership_check_for(backend),
                    None,
                    Probe::Failure(error),
                )),
                Candidate::Invalid {
                    backend,
                    target,
                    error,
                } => findings.push(finding(
                    DoctorSection::Filesystems,
                    ownership_check_for(backend),
                    target,
                    Probe::Failure(error),
                )),
                Candidate::Docker(owned) => findings.push(docker_candidate_finding(
                    owned,
                    self.control_status().health.overall_state(),
                )),
                Candidate::Host {
                    state_dir,
                    record,
                    confirmed,
                } => {
                    let owned = filesystem_ownership(&self.state, &record.filesystem)
                        .await?
                        .is_some();
                    let (finding, remediation) = self
                        .host_candidate_finding(paths, state_dir, record, confirmed, owned, live)
                        .await?;
                    if let Some(finding) = finding {
                        findings.push(finding);
                    }
                    if let Some(remediation) = remediation {
                        remediations.push(remediation);
                    }
                },
                Candidate::Libkrun {
                    filesystem,
                    state_dir,
                    confirmed,
                } => {
                    let owned = filesystem_ownership(&self.state, &filesystem)
                        .await?
                        .is_some();
                    let (finding, remediation) = self
                        .libkrun_candidate_finding(filesystem, state_dir, confirmed, owned, live)
                        .await?;
                    if let Some(finding) = finding {
                        findings.push(finding);
                    }
                    if let Some(remediation) = remediation {
                        remediations.push(remediation);
                    }
                },
            }
        }
        Ok((findings, remediations))
    }

    async fn host_candidate_finding(
        &self,
        paths: &RuntimePaths,
        state_dir: PathBuf,
        record: omnifs_mtab::RunnerRecord,
        confirmed: std::result::Result<omnifs_thin::host_control::RunnerPhase, String>,
        owned: bool,
        live: &[omnifs_api::FilesystemDefinition],
    ) -> Result<(Option<DoctorFinding>, Option<DoctorRemediation>)> {
        let name = record.filesystem.clone();
        let attached = live
            .iter()
            .any(|filesystem| filesystem.name == name && filesystem.spec == record.spec);
        let target = Some(format!(
            "`{}` {}/host at {}",
            name,
            record.spec.protocol(),
            record.spec.location().display()
        ));
        match confirmed {
            Ok(_phase) if attached => Ok((None, None)),
            Ok(phase) => {
                let (finding, remediation) = self
                    .mint_filesystem_offer(paths, target, name, state_dir, record, owned)
                    .await?;
                Ok((
                    Some(DoctorFinding {
                        section: DoctorSection::Filesystems,
                        check: DoctorCheckKind::StrayFilesystem,
                        target: finding,
                        severity: DoctorSeverity::Attention,
                        message: host_stray_message(
                            phase,
                            self.control_status().health.overall_state(),
                        ),
                        fix: remediation.as_ref().map(|value| value.command_line.clone()),
                        remediation_id: remediation.as_ref().map(|value| value.id.clone()),
                    }),
                    remediation,
                ))
            },
            Err(error) => {
                let mount_active = match omnifs_nfs::mount_is_active_checked(record.spec.location())
                {
                    Ok(active) => active,
                    Err(error) => {
                        return Ok((
                            Some(finding(
                                DoctorSection::Filesystems,
                                DoctorCheckKind::FilesystemState,
                                None,
                                Probe::Failure(format!("{error:#}")),
                            )),
                            None,
                        ));
                    },
                };
                let (fix, remediation) = if !mount_active && !attached && !owned {
                    let command_line = format!(
                        "omnifs doctor (clean stale host record for {})",
                        record.spec.location().display()
                    );
                    let offer = self
                        .doctor
                        .mint(
                            command_line.clone(),
                            DoctorExecutor::Daemon,
                            DoctorRepair::CleanupHost {
                                paths: paths.clone(),
                                state_dir: state_dir.clone(),
                                record: record.clone(),
                            },
                        )
                        .await?;
                    (Some(command_line), Some(offer))
                } else {
                    (None, None)
                };
                Ok((
                    Some(DoctorFinding {
                        section: DoctorSection::Filesystems,
                        check: DoctorCheckKind::StaleFilesystemState,
                        target,
                        severity: if mount_active || attached {
                            DoctorSeverity::Failure
                        } else {
                            DoctorSeverity::Attention
                        },
                        message: if attached {
                            format!(
                                "runner control cannot be confirmed but the daemon still reports it attached: {error}"
                            )
                        } else if mount_active {
                            format!("runner cannot be confirmed but its mount is active: {error}")
                        } else {
                            format!("runner cannot be confirmed: {error}")
                        },
                        fix: fix.clone(),
                        remediation_id: remediation.as_ref().map(|value| value.id.clone()),
                    }),
                    remediation,
                ))
            },
        }
    }

    async fn libkrun_candidate_finding(
        &self,
        name: ResourceName,
        state_dir: PathBuf,
        confirmed: std::result::Result<Option<omnifs_libkrun::HelperRecord>, String>,
        owned: bool,
        live: &[omnifs_api::FilesystemDefinition],
    ) -> Result<(Option<DoctorFinding>, Option<DoctorRemediation>)> {
        match confirmed {
            Ok(Some(record)) if record.filesystem != name => Ok((
                Some(finding(
                    DoctorSection::Filesystems,
                    DoctorCheckKind::LibkrunFilesystemOwnership,
                    Some(name.to_string()),
                    Probe::Failure(format!(
                        "helper claims filesystem `{}` instead of matching its state path",
                        record.filesystem
                    )),
                )),
                None,
            )),
            Ok(Some(record)) => {
                let attached = live
                    .iter()
                    .any(|filesystem| filesystem.name == name && filesystem.spec == record.spec);
                if attached {
                    return Ok((None, None));
                }
                let command_line = format!("omnifs fs rm {name}");
                let remediation = if owned {
                    None
                } else {
                    Some(
                        self.doctor
                            .mint(
                                command_line.clone(),
                                DoctorExecutor::Daemon,
                                DoctorRepair::StopLibkrun {
                                    state_dir,
                                    record: record.clone(),
                                },
                            )
                            .await?,
                    )
                };
                Ok((
                    Some(DoctorFinding {
                        section: DoctorSection::Filesystems,
                        check: DoctorCheckKind::StrayFilesystem,
                        target: Some(name.to_string()),
                        severity: DoctorSeverity::Attention,
                        message: libkrun_stray_message(
                            self.control_status().health.overall_state(),
                        ),
                        fix: remediation.as_ref().map(|value| value.command_line.clone()),
                        remediation_id: remediation.as_ref().map(|value| value.id.clone()),
                    }),
                    remediation,
                ))
            },
            Ok(None) => Ok((None, None)),
            Err(error) => Ok((
                Some(finding(
                    DoctorSection::Filesystems,
                    DoctorCheckKind::LibkrunFilesystemOwnership,
                    Some(name.to_string()),
                    Probe::Failure(error),
                )),
                None,
            )),
        }
    }

    async fn mint_filesystem_offer(
        &self,
        paths: &RuntimePaths,
        target: Option<String>,
        name: ResourceName,
        state_dir: PathBuf,
        record: omnifs_mtab::RunnerRecord,
        owned: bool,
    ) -> Result<(Option<String>, Option<DoctorRemediation>)> {
        if owned {
            return Ok((target, None));
        }
        let command_line = format!("omnifs fs rm {name}");
        let remediation = self
            .doctor
            .mint(
                command_line,
                DoctorExecutor::Daemon,
                DoctorRepair::StopHost {
                    paths: paths.clone(),
                    state_dir,
                    record,
                },
            )
            .await?;
        Ok((target, Some(remediation)))
    }
}

#[derive(Debug, PartialEq, Eq)]
enum RepairResult {
    Applied,
    Skipped(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilesystemOwnership {
    Desired,
    Observed,
}

impl FilesystemOwnership {
    const fn skip_message(self) -> &'static str {
        match self {
            Self::Desired => "filesystem became desired since diagnosis",
            Self::Observed => "filesystem became observed since diagnosis",
        }
    }
}

async fn filesystem_ownership(
    state: &StateStore,
    name: &ResourceName,
) -> Result<Option<FilesystemOwnership>> {
    let desired = state.desired_filesystems().await?;
    let instances = state.filesystem_instances().await?;
    if desired
        .iter()
        .any(|filesystem| filesystem.definition.name == *name)
    {
        return Ok(Some(FilesystemOwnership::Desired));
    }
    Ok(instances
        .iter()
        .any(|instance| instance.name == *name)
        .then_some(FilesystemOwnership::Observed))
}

async fn apply_with_ownership_gate<F, Fut>(
    state: &StateStore,
    name: &ResourceName,
    effect: F,
) -> Result<RepairResult>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<RepairResult>>,
{
    if let Some(owner) = filesystem_ownership(state, name).await? {
        return Ok(RepairResult::Skipped(owner.skip_message().to_owned()));
    }
    effect().await
}

fn ownership_check_for(backend: &str) -> DoctorCheckKind {
    match backend {
        "docker" => DoctorCheckKind::DockerFilesystemOwnership,
        "libkrun" => DoctorCheckKind::LibkrunFilesystemOwnership,
        _ => DoctorCheckKind::FilesystemState,
    }
}

fn docker_candidate_finding(owned: OwnedFilesystemContainer, health: HealthState) -> DoctorFinding {
    finding_with_target(
        DoctorSection::Filesystems,
        DoctorCheckKind::DockerFilesystemOwnership,
        Some(owned.filesystem_id),
        Probe::Attention(format!(
            "container {} cannot be remediated automatically: its record has no exact filesystem spec or runtime instance (daemon health is {})",
            owned.identity.id,
            legacy_health_label(health)
        )),
    )
}

fn legacy_health_label(health: HealthState) -> &'static str {
    match health {
        HealthState::Healthy => "Running",
        HealthState::Starting => "Starting",
        HealthState::Degraded => "Degraded",
        HealthState::Unhealthy => "Failed",
    }
}

fn host_stray_message(
    phase: omnifs_thin::host_control::RunnerPhase,
    health: HealthState,
) -> String {
    format!(
        "runner is confirmed in phase {phase:?} but daemon health is {} and reports no matching filesystem",
        legacy_health_label(health)
    )
}

fn libkrun_stray_message(health: HealthState) -> String {
    format!(
        "helper identity is confirmed but daemon health is {} and reports no matching filesystem",
        legacy_health_label(health)
    )
}

fn mount_auth_finding(mount: &omnifs_api::MountRecord) -> Option<(DoctorSeverity, String)> {
    if mount.definition.auth.is_none() {
        return None;
    }
    match (mount.auth_health, &mount.health) {
        (Some(CredentialHealth::Expired), _) => {
            Some((DoctorSeverity::Attention, "token expired".to_owned()))
        },
        (Some(CredentialHealth::RefreshFailed), _) => Some((
            DoctorSeverity::Failure,
            "credential refresh failed".to_owned(),
        )),
        (Some(CredentialHealth::NeedsConsent), _) => Some((
            DoctorSeverity::Failure,
            "credential needs consent".to_owned(),
        )),
        (Some(CredentialHealth::Missing) | None, MountHealth::AuthRequired) => {
            Some((DoctorSeverity::Attention, "credential missing".to_owned()))
        },
        _ => None,
    }
}

fn finding(
    section: DoctorSection,
    check: DoctorCheckKind,
    target: Option<String>,
    probe: Probe,
) -> DoctorFinding {
    let (severity, message) = probe.into_parts();
    finding_with_target_and_message(section, check, target, severity, message)
}

fn finding_with_target(
    section: DoctorSection,
    check: DoctorCheckKind,
    target: Option<String>,
    probe: Probe,
) -> DoctorFinding {
    finding(section, check, target, probe)
}

fn finding_with_target_and_message(
    section: DoctorSection,
    check: DoctorCheckKind,
    target: Option<String>,
    severity: DoctorSeverity,
    message: String,
) -> DoctorFinding {
    DoctorFinding {
        section,
        check,
        target,
        severity,
        message,
        fix: None,
        remediation_id: None,
    }
}

enum Probe {
    Positive(String),
    Neutral(String),
    Attention(String),
    Failure(String),
}

impl Probe {
    fn into_parts(self) -> (DoctorSeverity, String) {
        match self {
            Self::Positive(message) => (DoctorSeverity::Positive, message),
            Self::Neutral(message) => (DoctorSeverity::Neutral, message),
            Self::Attention(message) => (DoctorSeverity::Attention, message),
            Self::Failure(message) => (DoctorSeverity::Failure, message),
        }
    }
}

fn probe_fuse() -> DoctorFinding {
    #[cfg(target_os = "linux")]
    {
        let path = Path::new("/dev/fuse");
        if !path.exists() {
            return finding(
                DoctorSection::Environment,
                DoctorCheckKind::Fuse,
                None,
                Probe::Failure("/dev/fuse does not exist".to_owned()),
            );
        }
        return match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
        {
            Ok(_) => finding(
                DoctorSection::Environment,
                DoctorCheckKind::Fuse,
                None,
                Probe::Positive("/dev/fuse openable".to_owned()),
            ),
            Err(error) => finding(
                DoctorSection::Environment,
                DoctorCheckKind::Fuse,
                None,
                Probe::Failure(format!("/dev/fuse open: {error}")),
            ),
        };
    }
    #[cfg(not(target_os = "linux"))]
    {
        finding(
            DoctorSection::Environment,
            DoctorCheckKind::Fuse,
            None,
            Probe::Neutral(
                "macOS: native mount is NFS loopback; FUSE runs only inside the optional filesystem container"
                    .to_owned(),
            ),
        )
    }
}

fn probe_ssh_agent() -> DoctorFinding {
    let path = std::env::var_os("SSH_AUTH_SOCK").map(PathBuf::from);
    let exists = path.as_deref().is_some_and(Path::exists);
    probe_ssh_agent_state(path.as_deref(), exists)
}

fn probe_ssh_agent_state(path: Option<&Path>, exists: bool) -> DoctorFinding {
    let probe = match (path, exists) {
        (Some(path), true) => Probe::Positive(format!("{} (daemon environment)", path.display())),
        (Some(_), false) => Probe::Attention(
            "SSH_AUTH_SOCK set but socket not found (daemon environment)".to_owned(),
        ),
        (None, _) => Probe::Attention(
            "SSH_AUTH_SOCK unset; git callouts will fail (daemon environment)".to_owned(),
        ),
    };
    finding(
        DoctorSection::Profile,
        DoctorCheckKind::SshAgent,
        None,
        probe,
    )
}

fn credential_count(count: usize) -> String {
    format!("{count} credential{}", if count == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_api::{FilesystemDefinition, NormalizedResourceSet, ResourceDefinition};
    use omnifs_core::{
        FilesystemProtocol, FilesystemRuntime, FilesystemSpec, MutationId, ResourceName,
    };
    use omnifs_state::{
        DaemonStatePaths, FilesystemObservation, FilesystemPhase, ResourceApplyRequest,
        StateStoreOptions,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn resource_name(value: &str) -> ResourceName {
        ResourceName::new(value).unwrap()
    }

    fn filesystem_spec(name: &str) -> FilesystemSpec {
        let protocol = if cfg!(target_os = "linux") {
            FilesystemProtocol::Fuse
        } else {
            FilesystemProtocol::Nfs
        };
        FilesystemSpec::new(
            protocol,
            FilesystemRuntime::Host,
            format!("/tmp/omnifs-doctor-{name}").into(),
            None,
            None,
        )
        .unwrap()
    }

    fn filesystem_set(names: &[&str]) -> NormalizedResourceSet {
        NormalizedResourceSet::new(
            names
                .iter()
                .map(|name| {
                    ResourceDefinition::Filesystem(FilesystemDefinition {
                        name: resource_name(name),
                        spec: filesystem_spec(name),
                    })
                })
                .collect(),
        )
        .unwrap()
    }

    async fn apply_desired(state: &StateStore, desired: NormalizedResourceSet, mutation: u8) {
        let snapshot = state.resource_snapshot().await.unwrap();
        state
            .apply_resources(ResourceApplyRequest {
                mutation_id: MutationId::from_bytes([mutation; 16]),
                base_revision: snapshot.revision,
                expected_desired_digest: desired.digest(),
                desired,
                credential_secrets: Vec::new(),
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn state_store_ownership_covers_pending_starting_and_deleting_instances() {
        let temp = tempfile::tempdir().unwrap();
        let state = StateStore::open(
            DaemonStatePaths::new(temp.path().join("daemon-state")),
            StateStoreOptions::default(),
        )
        .await
        .unwrap();
        apply_desired(
            &state,
            filesystem_set(&["desired", "pending", "starting", "deleting"]),
            0x61,
        )
        .await;

        let pending = state
            .filesystem_instance(&resource_name("pending"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pending.phase, FilesystemPhase::Pending);

        let starting_name = resource_name("starting");
        let starting = state
            .filesystem_instance(&starting_name)
            .await
            .unwrap()
            .unwrap();
        let mut starting_observation = FilesystemObservation::from_instance(&starting);
        starting_observation.phase = FilesystemPhase::Starting;
        assert!(
            state
                .write_filesystem_observation(starting_observation)
                .await
                .unwrap()
                .is_some()
        );

        apply_desired(
            &state,
            filesystem_set(&["desired", "pending", "starting"]),
            0x62,
        )
        .await;
        let deleting_name = resource_name("deleting");
        let deleting = state
            .filesystem_instance(&deleting_name)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(deleting.phase, FilesystemPhase::Deleting);
        assert!(deleting.deleting);

        assert_eq!(
            filesystem_ownership(&state, &resource_name("desired"))
                .await
                .unwrap(),
            Some(FilesystemOwnership::Desired)
        );
        assert_eq!(
            filesystem_ownership(&state, &resource_name("pending"))
                .await
                .unwrap(),
            Some(FilesystemOwnership::Desired)
        );
        assert_eq!(
            filesystem_ownership(&state, &starting_name).await.unwrap(),
            Some(FilesystemOwnership::Desired)
        );
        assert_eq!(
            filesystem_ownership(&state, &deleting_name).await.unwrap(),
            Some(FilesystemOwnership::Observed)
        );
        state.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn apply_recheck_skips_state_inserted_after_offer_claim() {
        let temp = tempfile::tempdir().unwrap();
        let state = StateStore::open(
            DaemonStatePaths::new(temp.path().join("daemon-state")),
            StateStoreOptions::default(),
        )
        .await
        .unwrap();
        let doctor = DoctorState::new();

        let desired_name = resource_name("desired-after-claim");
        let desired_offer = doctor
            .mint(
                "omnifs fs rm desired-after-claim".to_owned(),
                DoctorExecutor::Daemon,
                DoctorRepair::ClientMountReauth,
            )
            .await
            .unwrap();
        assert!(
            doctor
                .claim(std::slice::from_ref(&desired_offer.id))
                .await
                .first()
                .and_then(|(_, offer)| offer.as_ref())
                .is_some()
        );
        apply_desired(&state, filesystem_set(&["desired-after-claim"]), 0x63).await;
        let effects = Arc::new(AtomicUsize::new(0));
        let desired_effects = Arc::clone(&effects);
        let result = apply_with_ownership_gate(&state, &desired_name, || async move {
            desired_effects.fetch_add(1, Ordering::SeqCst);
            Ok(RepairResult::Applied)
        })
        .await
        .unwrap();
        assert_eq!(
            result,
            RepairResult::Skipped("filesystem became desired since diagnosis".to_owned())
        );
        assert_eq!(effects.load(Ordering::SeqCst), 0);

        let observed_name = resource_name("observed-after-claim");
        let observed_offer = doctor
            .mint(
                "omnifs fs rm observed-after-claim".to_owned(),
                DoctorExecutor::Daemon,
                DoctorRepair::ClientMountReauth,
            )
            .await
            .unwrap();
        assert!(
            doctor
                .claim(std::slice::from_ref(&observed_offer.id))
                .await
                .first()
                .and_then(|(_, offer)| offer.as_ref())
                .is_some()
        );
        apply_desired(&state, filesystem_set(&["observed-after-claim"]), 0x64).await;
        apply_desired(&state, NormalizedResourceSet::empty(), 0x65).await;
        let observed_effects = Arc::clone(&effects);
        let result = apply_with_ownership_gate(&state, &observed_name, || async move {
            observed_effects.fetch_add(1, Ordering::SeqCst);
            Ok(RepairResult::Applied)
        })
        .await
        .unwrap();
        assert_eq!(
            result,
            RepairResult::Skipped("filesystem became observed since diagnosis".to_owned())
        );
        assert_eq!(effects.load(Ordering::SeqCst), 0);
        state.shutdown().await.unwrap();
    }

    #[test]
    fn base_findings_retain_legacy_order() {
        assert_eq!(
            BASE_FINDING_ORDER,
            [
                DoctorCheckKind::Docker,
                DoctorCheckKind::Fuse,
                DoctorCheckKind::Image,
                DoctorCheckKind::CredentialStore,
                DoctorCheckKind::SshAgent,
                DoctorCheckKind::Config,
                DoctorCheckKind::Network,
            ]
        );
    }

    #[test]
    fn legacy_health_words_cover_all_states_and_backends() {
        let owned = OwnedFilesystemContainer {
            identity: DockerContainerIdentity {
                id: "container".to_owned(),
                runtime_instance: "runtime".to_owned(),
            },
            filesystem_id: "filesystem".to_owned(),
            names: Vec::new(),
        };
        for (health, expected) in [
            (HealthState::Healthy, "Running"),
            (HealthState::Starting, "Starting"),
            (HealthState::Degraded, "Degraded"),
            (HealthState::Unhealthy, "Failed"),
        ] {
            assert_eq!(legacy_health_label(health), expected);
            assert!(
                docker_candidate_finding(owned.clone(), health)
                    .message
                    .contains(&format!("daemon health is {expected}"))
            );
            assert!(
                host_stray_message(omnifs_thin::host_control::RunnerPhase::Mounted, health)
                    .contains(&format!("daemon health is {expected}"))
            );
            assert!(
                libkrun_stray_message(health).contains(&format!("daemon health is {expected}"))
            );
        }
    }

    #[test]
    fn ssh_agent_severity_uses_probe_state_not_message_words() {
        let path = Path::new("/tmp/socket-not-found-unset");
        let present = probe_ssh_agent_state(Some(path), true);
        assert_eq!(present.severity, DoctorSeverity::Positive);
        assert!(present.message.contains(path.to_string_lossy().as_ref()));

        let missing = probe_ssh_agent_state(Some(path), false);
        assert_eq!(missing.severity, DoctorSeverity::Attention);
        let unset = probe_ssh_agent_state(None, false);
        assert_eq!(unset.severity, DoctorSeverity::Attention);
    }

    #[test]
    fn ownership_scan_has_one_three_second_aggregate_budget() {
        assert_eq!(OWNERSHIP_SCAN_TIMEOUT, Duration::from_secs(3));
    }

    #[test]
    fn unknown_remediation_text_is_stable() {
        assert_eq!(
            UNKNOWN_REMEDIATION,
            "remediation id unknown or no longer offered"
        );
    }

    #[tokio::test]
    async fn unknown_and_expired_offers_are_not_claimed() {
        let state = DoctorState::new();
        let unknown = state.claim(&["missing".to_owned()]).await;
        assert_eq!(unknown.len(), 1);
        assert!(unknown[0].1.is_none());

        state.offers.lock().await.insert(
            "expired".to_owned(),
            StoredOffer {
                command_line: "omnifs doctor".to_owned(),
                executor: DoctorExecutor::Daemon,
                repair: DoctorRepair::ClientMountReauth,
                expires_at: Instant::now() - Duration::from_secs(1),
            },
        );
        let expired = state.claim(&["expired".to_owned()]).await;
        assert_eq!(expired.len(), 1);
        assert!(expired[0].1.is_none());
    }
}
