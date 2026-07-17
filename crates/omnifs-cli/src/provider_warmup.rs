//! Detached provider warmup and foreground joining for daemon startup.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use atomic_write_file::OpenOptions as AtomicOpenOptions;
use clap::Args;
use fs2::FileExt as _;
use futures_util::StreamExt as _;
use omnifs_workspace::ids::ProviderId;
use omnifs_workspace::layout::{WorkspaceLayout, wasm_cache_dir};
use omnifs_workspace::provider::Catalog;
use serde::{Deserialize, Serialize};

use crate::ui::output::Output;
use crate::workspace::Workspace;

const CHILD_READY_POLL: Duration = Duration::from_millis(10);
const CHILD_READY_TIMEOUT: Duration = Duration::from_secs(5);
const LOCK_FILE: &str = "provider-warmup.lock";
const PROGRESS_FILE: &str = "provider-warmup.json";

#[derive(Args, Debug, Clone)]
pub(crate) struct WarmProvidersArgs {
    #[arg(long)]
    id: ProviderId,
}

impl WarmProvidersArgs {
    pub(crate) async fn run(self) -> Result<()> {
        Workspace::resolve()?
            .provider_warmup()
            .run_background(self.id)
            .await
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum WarmupState {
    Running,
    Complete,
    Failed,
    Interrupted,
    Unreadable,
}

impl WarmupState {
    const fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Complete => "complete",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
            Self::Unreadable => "unreadable",
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct WarmupStatus {
    state: WarmupState,
    completed: usize,
    total: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WarmupProgress {
    pid: u32,
    completed: usize,
    total: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl WarmupStatus {
    fn unreadable(error: &impl std::fmt::Display) -> Self {
        Self {
            state: WarmupState::Unreadable,
            completed: 0,
            total: 0,
            error: Some(error.to_string()),
        }
    }

    pub(crate) fn summary(&self) -> String {
        if self.total == 0 {
            self.state.label().to_owned()
        } else {
            format!("{}/{} {}", self.completed, self.total, self.state.label())
        }
    }
}

/// Provider warmup for one resolved workspace.
#[derive(Clone)]
pub(crate) struct ProviderWarmup {
    layout: WorkspaceLayout,
}

/// Exclusive warmup ownership retained by `omnifs up` through daemon readiness.
#[must_use = "dropping the lease allows detached provider warmup to resume"]
pub(crate) struct WarmupLease {
    _lock: File,
}

impl ProviderWarmup {
    pub(crate) fn new(layout: &WorkspaceLayout) -> Self {
        Self {
            layout: layout.clone(),
        }
    }

    pub(crate) fn status(&self) -> Option<WarmupStatus> {
        let progress = match self.read_progress() {
            Ok(Some(progress)) => progress,
            Ok(None) => return None,
            Err(error) => return Some(WarmupStatus::unreadable(&error)),
        };
        let state = match self.is_active() {
            Ok(true) => WarmupState::Running,
            Ok(false) if progress.error.is_some() => WarmupState::Failed,
            Ok(false) if progress.completed == progress.total => WarmupState::Complete,
            Ok(false) => WarmupState::Interrupted,
            Err(error) => return Some(WarmupStatus::unreadable(&error)),
        };
        Some(WarmupStatus {
            state,
            completed: progress.completed,
            total: progress.total,
            error: progress.error,
        })
    }

    /// Start a warmup process that survives this foreground command.
    pub(crate) fn spawn_background(&self, id: ProviderId, output: &Output) -> Result<()> {
        if self.is_active()? {
            output.narrate("Provider warmup is already running; `omnifs up` will join it.");
            return Ok(());
        }

        std::fs::create_dir_all(&self.layout.cache_dir).with_context(|| {
            format!("create cache directory {}", self.layout.cache_dir.display())
        })?;
        let binary = std::env::current_exe().context("resolve the omnifs executable")?;
        let mut command = std::process::Command::new(&binary);
        command
            .arg("warm-providers")
            .arg("--id")
            .arg(id.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env("OMNIFS_HOME", &self.layout.config_dir);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }

        let mut child = command
            .spawn()
            .with_context(|| format!("spawn provider warmup ({})", binary.display()))?;
        let child_pid = child.id();
        let deadline = Instant::now() + CHILD_READY_TIMEOUT;
        loop {
            let child_has_lock = self
                .read_progress()
                .ok()
                .flatten()
                .is_some_and(|progress| progress.pid == child_pid)
                && self.is_active()?;
            if child_has_lock {
                break;
            }
            if let Some(status) = child
                .try_wait()
                .context("observe provider warmup startup")?
            {
                if status.success() {
                    break;
                }
                bail!("provider warmup exited before becoming ready ({status})");
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!(
                    "provider warmup did not become ready within {}s",
                    CHILD_READY_TIMEOUT.as_secs()
                );
            }
            std::thread::sleep(CHILD_READY_POLL);
        }
        output.narrate(
            "Warming the provider runtime in the background; check `omnifs status` for progress.",
        );
        Ok(())
    }

    /// Join detached work, then warm the exact providers before daemon replacement.
    pub(crate) async fn warm_for_up(
        &self,
        ids: impl IntoIterator<Item = ProviderId>,
        output: &Output,
    ) -> Result<WarmupLease> {
        let ids: Vec<_> = ids
            .into_iter()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let mut progress = output.progress("provider warmup");
        if self.is_active()? {
            progress.update("waiting for background provider warmup");
        }
        // ponytail: one workspace lock; split by provider only if contention is measurable.
        let lock = tokio::task::spawn_blocking({
            let warmup = self.clone();
            move || warmup.acquire()
        })
        .await
        .context("join provider warmup lock task")??;
        progress.update("warming selected providers");
        let result = self.warm(ids).await;
        match &result {
            Ok(()) => progress.settle_ok("ready"),
            Err(_) => progress.settle_fail("warmup failed"),
        }
        result?;
        Ok(WarmupLease { _lock: lock })
    }

    async fn run_background(&self, id: ProviderId) -> Result<()> {
        let _lock = tokio::task::spawn_blocking({
            let warmup = self.clone();
            move || warmup.acquire()
        })
        .await
        .context("join provider warmup lock task")??;
        self.warm(vec![id]).await
    }

    async fn warm(&self, ids: Vec<ProviderId>) -> Result<()> {
        let total = ids.len();
        let mut progress = WarmupProgress {
            pid: std::process::id(),
            completed: 0,
            total,
            error: None,
        };
        self.write_progress(&progress)?;
        if total == 0 {
            return Ok(());
        }

        let catalog = Catalog::open(&self.layout.providers_dir);
        let providers = ids
            .into_iter()
            .map(|id| {
                catalog
                    .get(&id)
                    .with_context(|| format!("resolve retained provider {id}"))?
                    .with_context(|| format!("retained provider {id} is missing"))
            })
            .collect::<Result<Vec<_>>>();
        let providers = match providers {
            Ok(providers) => providers,
            Err(error) => {
                let message = format!("resolve providers for warmup: {error:#}");
                progress.error = Some(message.clone());
                self.write_progress(&progress)?;
                return Err(anyhow::anyhow!(message));
            },
        };
        let engine = match omnifs_engine::ComponentEngine::new(Some(&wasm_cache_dir(
            &self.layout.cache_dir,
        ))) {
            Ok(engine) => engine,
            Err(error) => {
                let message = format!("initialize provider component engine: {error:#}");
                progress.error = Some(message.clone());
                self.write_progress(&progress)?;
                return Err(anyhow::anyhow!(message));
            },
        };
        let mut outcomes = engine.warm(providers);

        let mut failures = Vec::new();
        while let Some(outcome) = outcomes.next().await {
            progress.completed += 1;
            if let Err(error) = outcome.result {
                let error = format!("{}: {error:#}", outcome.provider_id);
                failures.push(error);
                progress.error = Some(failures.join("; "));
            }
            self.write_progress(&progress)?;
        }
        if failures.is_empty() {
            Ok(())
        } else {
            bail!(
                "failed to warm {} provider(s): {}",
                failures.len(),
                failures.join("; ")
            )
        }
    }

    fn acquire(&self) -> io::Result<File> {
        std::fs::create_dir_all(&self.layout.cache_dir)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let file = options.open(self.layout.cache_dir.join(LOCK_FILE))?;
        file.lock_exclusive()?;
        Ok(file)
    }

    fn is_active(&self) -> io::Result<bool> {
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.layout.cache_dir.join(LOCK_FILE))
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        match file.try_lock_exclusive() {
            Ok(()) => {
                file.unlock()?;
                Ok(false)
            },
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(true),
            Err(error) => Err(error),
        }
    }

    fn read_progress(&self) -> io::Result<Option<WarmupProgress>> {
        let bytes = match std::fs::read(self.layout.cache_dir.join(PROGRESS_FILE)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    fn write_progress(&self, progress: &WarmupProgress) -> io::Result<()> {
        let bytes = serde_json::to_vec(progress).map_err(io::Error::other)?;
        let mut options = AtomicOpenOptions::new();
        #[cfg(unix)]
        {
            use atomic_write_file::unix::OpenOptionsExt as _;
            use std::os::unix::fs::OpenOptionsExt as _;
            options.preserve_mode(false).mode(0o600);
        }
        let mut file = options.open(self.layout.cache_dir.join(PROGRESS_FILE))?;
        file.write_all(&bytes)?;
        file.commit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::output::OutputMode;

    #[tokio::test(flavor = "multi_thread")]
    async fn foreground_warmup_joins_an_existing_worker_and_retains_the_lease() {
        let home = tempfile::tempdir().unwrap();
        let layout = WorkspaceLayout::under_root(home.path());
        std::fs::create_dir_all(&layout.providers_dir).unwrap();
        let artifact = omnifs_workspace::provider::Artifact::from_file(
            omnifs_itest::provider_wasm_path("test_provider.wasm"),
        )
        .unwrap();
        let id = artifact.id();
        omnifs_workspace::provider::ProviderStore::new(&layout.providers_dir)
            .retain(&artifact)
            .unwrap();
        let holder_state = ProviderWarmup::new(&layout);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _lock = holder_state.acquire().unwrap();
            holder_state
                .write_progress(&WarmupProgress {
                    pid: std::process::id(),
                    completed: 0,
                    total: 1,
                    error: None,
                })
                .unwrap();
            ready_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(250));
        });
        ready_rx.recv().unwrap();

        let started = Instant::now();
        let warmup = ProviderWarmup::new(&layout);
        let lease = warmup
            .warm_for_up([id], &Output::new(OutputMode::Human, true))
            .await
            .unwrap();
        holder.join().unwrap();

        assert!(started.elapsed() >= Duration::from_millis(200));
        assert!(warmup.is_active().unwrap());
        assert_eq!(warmup.status().unwrap().state, WarmupState::Running);
        drop(lease);
        assert!(!warmup.is_active().unwrap());
        assert_eq!(warmup.status().unwrap().state, WarmupState::Complete);
    }
}
