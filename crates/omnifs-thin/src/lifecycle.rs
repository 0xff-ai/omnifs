use crate::host_control::{HostControl, RunnerPhase, StopOutcome, StopRequest};
use omnifs_core::{AttachmentProtocol, AttachmentRuntime, AttachmentSpec, ResourceName};
use omnifs_vfs::{TeardownOutcome, TeardownRequest};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};

const MOUNT_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const MOUNT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const STOP_ATTEMPT_WAIT: Duration = Duration::from_secs(2);

pub(crate) struct Lifecycle {
    pub(crate) cancelled: Arc<AtomicBool>,
    pub(crate) phase: watch::Sender<RunnerPhase>,
    pub(crate) wire_teardown_tx: mpsc::Sender<TeardownRequest>,
    pub(crate) wire_teardown_rx: mpsc::Receiver<TeardownRequest>,
    pub(crate) host_stop_rx: mpsc::Receiver<StopRequest>,
    pub(crate) host_control: Option<HostControl>,
}

pub(crate) struct LifecycleConfig<'a> {
    pub(crate) attachment: &'a ResourceName,
    pub(crate) spec: &'a AttachmentSpec,
    pub(crate) state_dir: Option<&'a Path>,
    pub(crate) runner_control: Option<RunnerControlConfig>,
}

pub(crate) struct RunnerControlConfig {
    pub(crate) instance_id: String,
    pub(crate) socket: PathBuf,
}

impl Lifecycle {
    pub(crate) fn prepare(config: LifecycleConfig<'_>) -> anyhow::Result<Self> {
        let LifecycleConfig {
            spec,
            attachment,
            state_dir,
            runner_control,
        } = config;
        let (phase, _) = watch::channel(RunnerPhase::Preflight);
        let (wire_teardown_tx, wire_teardown_rx) = mpsc::channel(1);
        let (host_stop_tx, host_stop_rx) = mpsc::channel(1);
        let host_control = match spec.runtime() {
            AttachmentRuntime::Host => {
                let state_dir = state_dir
                    .ok_or_else(|| anyhow::anyhow!("host filesystem requires --state-dir"))?;
                let runner_control = runner_control.ok_or_else(|| {
                    anyhow::anyhow!(
                        "host filesystem requires --runner-instance and --runner-control"
                    )
                })?;
                let RunnerControlConfig {
                    instance_id,
                    socket,
                } = runner_control;
                let record = omnifs_mtab::RunnerRecord::new(
                    instance_id.clone(),
                    attachment.clone(),
                    spec.clone(),
                    socket,
                )?;
                let mut control = HostControl::bind(state_dir, &record)?;
                control.spawn(instance_id, phase.subscribe(), host_stop_tx);
                Some(control)
            },
            AttachmentRuntime::Docker | AttachmentRuntime::Libkrun => {
                anyhow::ensure!(
                    runner_control.is_none(),
                    "--runner-instance and --runner-control are host-only"
                );
                None
            },
        };
        Ok(Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            phase,
            wire_teardown_tx,
            wire_teardown_rx,
            host_stop_rx,
            host_control,
        })
    }
}

pub(crate) fn preflight(spec: &AttachmentSpec, state_dir: Option<&Path>) -> anyhow::Result<()> {
    let mount_point = spec.location();
    std::fs::create_dir_all(mount_point)?;
    if !omnifs_nfs::mount_is_active_checked(mount_point)? {
        return Ok(());
    }
    anyhow::ensure!(
        spec.protocol() == AttachmentProtocol::Nfs && omnifs_nfs::mount_is_omnifs(mount_point),
        "refusing to start a filesystem: {} is already mounted",
        mount_point.display()
    );
    let state_dir = state_dir.ok_or_else(|| {
        anyhow::anyhow!(
            "refusing to recover active NFS mount {} without state",
            mount_point.display()
        )
    })?;
    omnifs_mtab::MountState::read_unique(state_dir)?.nfs_addr_for(mount_point)?;
    Ok(())
}

pub(crate) async fn coordinate_mount(
    spec: &AttachmentSpec,
    lifecycle: &mut Lifecycle,
    mount_done: oneshot::Receiver<anyhow::Result<()>>,
) -> anyhow::Result<()> {
    MountCoordinator::new(
        spec.protocol(),
        spec.location().to_path_buf(),
        lifecycle,
        mount_done,
    )
    .run()
    .await
}

struct MountCoordinator<'a> {
    protocol: AttachmentProtocol,
    mount_point: PathBuf,
    lifecycle: &'a mut Lifecycle,
    mount_done: oneshot::Receiver<anyhow::Result<()>>,
    mount_result: Option<anyhow::Result<()>>,
    unmount_task: Option<tokio::task::JoinHandle<bool>>,
    startup_deadline: tokio::time::Instant,
    poll: tokio::time::Interval,
    signals: Signals,
    mounted: bool,
    stopping: bool,
    timed_out: bool,
}

impl<'a> MountCoordinator<'a> {
    fn new(
        protocol: AttachmentProtocol,
        mount_point: PathBuf,
        lifecycle: &'a mut Lifecycle,
        mount_done: oneshot::Receiver<anyhow::Result<()>>,
    ) -> Self {
        let mut poll = tokio::time::interval(MOUNT_POLL_INTERVAL);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        Self {
            protocol,
            mount_point,
            lifecycle,
            mount_done,
            mount_result: None,
            unmount_task: None,
            startup_deadline: tokio::time::Instant::now() + MOUNT_STARTUP_TIMEOUT,
            poll,
            signals: Signals::new(),
            mounted: false,
            stopping: false,
            timed_out: false,
        }
    }

    async fn run(mut self) -> anyhow::Result<()> {
        loop {
            let await_mount = self.mount_result.is_none();
            let host_control = self.lifecycle.host_control.is_some();
            tokio::select! {
                result = &mut self.mount_done, if await_mount => {
                    let result = result.unwrap_or_else(|_| {
                        Err(anyhow::anyhow!("mount owner exited without a result"))
                    });
                    if let Some(result) = self.record_completion(result) {
                        return result;
                    }
                }
                request = self.lifecycle.wire_teardown_rx.recv() => {
                    let Some(request) = request else { continue; };
                    self.stopping = true;
                    let stopped = self.stop_once(true).await;
                    request.complete(if stopped {
                        TeardownOutcome::Stopped
                    } else {
                        TeardownOutcome::Busy
                    });
                    if stopped {
                        return Ok(());
                    }
                }
                request = self.lifecycle.host_stop_rx.recv(), if host_control => {
                    let Some(request) = request else { continue; };
                    self.stopping = true;
                    let stopped = self.stop_once(true).await;
                    request.complete(self.host_stop_outcome(stopped)).await;
                    if stopped {
                        return Ok(());
                    }
                }
                signal = self.signals.recv() => {
                    if signal.is_none() {
                        continue;
                    }
                    self.stopping = true;
                    if self.stop_once(false).await {
                        return Ok(());
                    }
                }
                _instant = self.poll.tick() => {
                    self.observe_mount();
                    if self.stopping && self.stop_once(false).await {
                        return self.stopped_result();
                    }
                }
            }
        }
    }

    fn observe_mount(&mut self) {
        if !self.mounted && omnifs_nfs::mount_is_active(&self.mount_point) {
            self.mounted = true;
            self.lifecycle.phase.send_replace(RunnerPhase::Mounted);
        }
        if !self.mounted && !self.stopping && tokio::time::Instant::now() >= self.startup_deadline {
            self.stopping = true;
            self.timed_out = true;
        }
    }

    fn record_completion(&mut self, result: anyhow::Result<()>) -> Option<anyhow::Result<()>> {
        if !self.stopping {
            if let Err(error) = &result {
                self.lifecycle.phase.send_replace(RunnerPhase::Failed {
                    message: format!("{error:#}"),
                });
            }
            return Some(result);
        }
        self.mount_result = Some(result);
        if !omnifs_nfs::mount_is_active(&self.mount_point) && self.unmount_task.is_none() {
            return Some(self.stopped_result());
        }
        if let Some(Err(error)) = &self.mount_result {
            self.lifecycle.phase.send_replace(RunnerPhase::Failed {
                message: format!("{error:#}"),
            });
        }
        None
    }

    async fn stop_once(&mut self, wait: bool) -> bool {
        self.lifecycle.cancelled.store(true, Ordering::Release);
        self.lifecycle.phase.send_replace(RunnerPhase::Stopping);
        let active = omnifs_nfs::mount_is_active(&self.mount_point);
        if active && self.unmount_task.is_none() {
            let protocol = self.protocol;
            let mount = self.mount_point.clone();
            self.unmount_task = Some(tokio::task::spawn_blocking(move || {
                match protocol {
                    AttachmentProtocol::Nfs => {
                        omnifs_nfs::unmount(&mount).map_err(|error| error.to_string())
                    },
                    AttachmentProtocol::Fuse => {
                        #[cfg(target_os = "linux")]
                        {
                            omnifs_fuse::mount::unmount(&mount).map_err(|error| error.to_string())
                        }
                        #[cfg(not(target_os = "linux"))]
                        {
                            Err("FUSE is not supported on this platform".to_owned())
                        }
                    },
                }
                .is_ok()
            }));
        }
        let unmounted = self.collect_unmount(wait).await.unwrap_or(!active);
        self.observe_mount_result();
        let absent = !omnifs_nfs::mount_is_active(&self.mount_point);
        if self.mount_result.is_some() && self.unmount_task.is_none() && absent {
            return true;
        }
        if !unmounted || !absent {
            self.lifecycle.phase.send_replace(RunnerPhase::Busy);
        }
        false
    }

    async fn collect_unmount(&mut self, wait: bool) -> Option<bool> {
        let task = self.unmount_task.as_mut()?;
        if !wait && !task.is_finished() {
            return None;
        }
        let result = if wait {
            match tokio::time::timeout(STOP_ATTEMPT_WAIT, task).await {
                Ok(result) => result,
                Err(_) => return None,
            }
        } else {
            task.await
        };
        self.unmount_task = None;
        Some(result.unwrap_or(false))
    }

    fn observe_mount_result(&mut self) {
        if self.mount_result.is_some() {
            return;
        }
        match self.mount_done.try_recv() {
            Ok(result) => self.mount_result = Some(result),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {},
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                self.mount_result =
                    Some(Err(anyhow::anyhow!("mount owner exited without a result")));
            },
        }
    }

    fn host_stop_outcome(&self, stopped: bool) -> StopOutcome {
        if stopped {
            StopOutcome::Stopped
        } else {
            StopOutcome::Busy {
                message: format!(
                    "{} is still mounted or mount cleanup is still running",
                    self.mount_point.display()
                ),
            }
        }
    }

    fn stopped_result(&self) -> anyhow::Result<()> {
        if self.timed_out {
            Err(anyhow::anyhow!(
                "{} filesystem mount startup exceeded {}s",
                self.protocol,
                MOUNT_STARTUP_TIMEOUT.as_secs()
            ))
        } else {
            Ok(())
        }
    }
}

struct Signals {
    #[cfg(unix)]
    terminate: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    interrupt: Option<tokio::signal::unix::Signal>,
}

impl Signals {
    fn new() -> Self {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Self {
                terminate: signal(SignalKind::terminate()).ok(),
                interrupt: signal(SignalKind::interrupt()).ok(),
            }
        }
        #[cfg(not(unix))]
        {
            Self {}
        }
    }

    async fn recv(&mut self) -> Option<()> {
        #[cfg(unix)]
        {
            tokio::select! {
                signal = async {
                    match &mut self.terminate {
                        Some(signal) => signal.recv().await,
                        None => std::future::pending().await,
                    }
                } => signal,
                signal = async {
                    match &mut self.interrupt {
                        Some(signal) => signal.recv().await,
                        None => std::future::pending().await,
                    }
                } => signal,
            }
        }
        #[cfg(not(unix))]
        {
            std::future::pending().await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coordinator(
        lifecycle: &mut Lifecycle,
        mount_point: PathBuf,
    ) -> (MountCoordinator<'_>, oneshot::Sender<anyhow::Result<()>>) {
        let (mount_done, mount_done_rx) = oneshot::channel();
        (
            MountCoordinator::new(
                AttachmentProtocol::Nfs,
                mount_point,
                lifecycle,
                mount_done_rx,
            ),
            mount_done,
        )
    }

    fn guest_lifecycle() -> Lifecycle {
        let attachment = ResourceName::new("test").unwrap();
        let spec = AttachmentSpec::new(
            AttachmentProtocol::Fuse,
            AttachmentRuntime::Docker,
            PathBuf::from(omnifs_core::ATTACHMENT_GUEST_LOCATION),
            None,
            None,
        )
        .unwrap();
        Lifecycle::prepare(LifecycleConfig {
            attachment: &attachment,
            spec: &spec,
            state_dir: None,
            runner_control: None,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn stop_waits_for_the_mount_owner_even_when_the_mount_is_absent() {
        let temp = tempfile::tempdir().unwrap();
        let mount_point = temp.path().join("mount");
        let mut lifecycle = guest_lifecycle();
        let (mut coordinator, mount_done) = coordinator(&mut lifecycle, mount_point);
        coordinator.stopping = true;

        assert!(!coordinator.stop_once(false).await);
        mount_done.send(Ok(())).unwrap();
        tokio::task::yield_now().await;
        assert!(coordinator.stop_once(false).await);
    }

    #[tokio::test]
    async fn an_unjoined_unmount_task_keeps_the_runner_alive() {
        let temp = tempfile::tempdir().unwrap();
        let mount_point = temp.path().join("mount");
        let mut lifecycle = guest_lifecycle();
        let (mut coordinator, _mount_done) = coordinator(&mut lifecycle, mount_point);
        coordinator.stopping = true;
        coordinator.mount_result = Some(Ok(()));
        coordinator.unmount_task = Some(tokio::spawn(std::future::pending::<bool>()));

        assert!(!coordinator.stop_once(false).await);
        coordinator
            .unmount_task
            .take()
            .expect("in-flight unmount task")
            .abort();
    }

    #[tokio::test]
    async fn a_finished_unmount_task_is_joined_before_stop_completes() {
        let temp = tempfile::tempdir().unwrap();
        let mount_point = temp.path().join("mount");
        let mut lifecycle = guest_lifecycle();
        let (mut coordinator, _mount_done) = coordinator(&mut lifecycle, mount_point);
        coordinator.stopping = true;
        coordinator.mount_result = Some(Ok(()));
        coordinator.unmount_task = Some(tokio::spawn(async { true }));
        tokio::task::yield_now().await;

        assert!(coordinator.stop_once(false).await);
        assert!(coordinator.unmount_task.is_none());
    }
}
