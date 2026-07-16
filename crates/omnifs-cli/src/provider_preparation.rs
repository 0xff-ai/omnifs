//! Detached provider compilation and foreground joining for daemon startup.

use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use clap::Args;
use fs2::FileExt as _;
use futures_util::StreamExt as _;
use omnifs_workspace::layout::{WorkspaceLayout, wasm_cache_dir};
use omnifs_workspace::provider::ProviderStore;
use serde::{Deserialize, Serialize};

use crate::ui::output::Output;

const CHILD_READY_POLL: Duration = Duration::from_millis(10);
const CHILD_READY_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PARALLELISM: usize = 4;
const LOCK_FILE: &str = "provider-preparation.lock";
const PROGRESS_FILE: &str = "provider-preparation.jsonl";

#[derive(Args, Debug, Clone, Default)]
pub(crate) struct PrepareProvidersArgs {}

impl PrepareProvidersArgs {
    pub(crate) async fn run(self) -> Result<()> {
        Preparation::new(&WorkspaceLayout::resolve()?)
            .run_background()
            .await
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct Status {
    state: &'static str,
    completed: usize,
    total: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Progress {
    pid: u32,
    completed: usize,
    total: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl Status {
    fn unreadable(error: &impl std::fmt::Display) -> Self {
        Self {
            state: "unreadable",
            completed: 0,
            total: 0,
            error: Some(error.to_string()),
        }
    }

    pub(crate) fn summary(&self) -> String {
        if self.total == 0 {
            self.state.to_owned()
        } else {
            format!("{}/{} {}", self.completed, self.total, self.state)
        }
    }
}

/// Provider preparation for one resolved workspace.
#[derive(Clone)]
pub(crate) struct Preparation {
    layout: WorkspaceLayout,
}

impl Preparation {
    pub(crate) fn new(layout: &WorkspaceLayout) -> Self {
        Self {
            layout: layout.clone(),
        }
    }

    pub(crate) fn status(&self) -> Option<Status> {
        let progress = match self.read_progress() {
            Ok(Some(progress)) => progress,
            Ok(None) => return None,
            Err(error) => return Some(Status::unreadable(&error)),
        };
        let state = match self.is_active() {
            Ok(true) => "running",
            Ok(false) if progress.error.is_some() => "failed",
            Ok(false) if progress.completed == progress.total => "complete",
            Ok(false) => "interrupted",
            Err(error) => return Some(Status::unreadable(&error)),
        };
        Some(Status {
            state,
            completed: progress.completed,
            total: progress.total,
            error: progress.error,
        })
    }

    /// Start a compiler process that survives this foreground command.
    pub(crate) fn spawn_background(&self, output: &Output) -> Result<()> {
        if self.is_active()? {
            output.narrate(
                "Provider runtime preparation is already running; `omnifs up` will join it.",
            );
            return Ok(());
        }

        std::fs::create_dir_all(&self.layout.cache_dir).with_context(|| {
            format!("create cache directory {}", self.layout.cache_dir.display())
        })?;
        let binary = std::env::current_exe().context("resolve the omnifs executable")?;
        let mut command = std::process::Command::new(&binary);
        command
            .arg("prepare-providers")
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
            .with_context(|| format!("spawn provider preparation ({})", binary.display()))?;
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
                .context("observe provider preparation startup")?
            {
                if status.success() {
                    break;
                }
                bail!("provider preparation exited before becoming ready ({status})");
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!(
                    "provider preparation did not become ready within {}s",
                    CHILD_READY_TIMEOUT.as_secs()
                );
            }
            std::thread::sleep(CHILD_READY_POLL);
        }
        output.narrate(
            "Preparing the provider runtime in the background; check `omnifs status` for progress.",
        );
        Ok(())
    }

    /// Wait for detached preparation before the daemon performs its normal load.
    pub(crate) async fn wait(&self, output: &Output) -> Result<()> {
        if !self.is_active()? {
            return Ok(());
        }
        let mut progress = output.progress("providers");
        progress.update("waiting for background preparation");
        let lock = tokio::task::spawn_blocking({
            let preparation = self.clone();
            move || preparation.acquire()
        })
        .await
        .context("join provider preparation lock task")??;
        drop(lock);
        progress.settle_ok("ready");
        Ok(())
    }

    async fn run_background(&self) -> Result<()> {
        let lock = tokio::task::spawn_blocking({
            let preparation = self.clone();
            move || preparation.acquire()
        })
        .await
        .context("join provider preparation lock task")??;
        let ids = ProviderStore::new(&self.layout.providers_dir)
            .read_index()?
            .providers
            .into_iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>();
        self.compile(&lock, ids).await
    }

    async fn compile(
        &self,
        lock: &File,
        ids: Vec<omnifs_workspace::ids::ProviderId>,
    ) -> Result<()> {
        let total = ids.len();
        let mut progress = Progress {
            pid: std::process::id(),
            completed: 0,
            total,
            error: None,
        };
        self.write_progress(lock, &progress)?;
        if total == 0 {
            return Ok(());
        }

        let compiler = match omnifs_engine::ComponentCompiler::new(
            &wasm_cache_dir(&self.layout.cache_dir),
            &self.layout.providers_dir,
        ) {
            Ok(compiler) => compiler,
            Err(error) => {
                let message = format!("initialize provider compiler: {error:#}");
                progress.error = Some(message.clone());
                self.write_progress(lock, &progress)?;
                return Err(anyhow::anyhow!(message));
            },
        };
        let parallelism = std::thread::available_parallelism()
            .map_or(1, std::num::NonZeroUsize::get)
            .min(MAX_PARALLELISM)
            .min(total);
        let jobs = futures_util::stream::iter(ids.into_iter().map(|id| {
            let compiler = compiler.clone();
            async move {
                let result = tokio::task::spawn_blocking(move || compiler.prepare(&id)).await;
                match result {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(format!("{id}: {error:#}")),
                    Err(error) => Some(format!("{id}: provider compiler task failed: {error}")),
                }
            }
        }))
        .buffer_unordered(parallelism);
        tokio::pin!(jobs);

        let mut failures = Vec::new();
        while let Some(error) = jobs.next().await {
            progress.completed += 1;
            if let Some(error) = error {
                failures.push(error);
                progress.error = Some(failures.join("; "));
            }
            self.write_progress(lock, &progress)?;
        }
        if failures.is_empty() {
            Ok(())
        } else {
            bail!(
                "failed to prepare {} provider(s): {}",
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

    fn read_progress(&self) -> io::Result<Option<Progress>> {
        let contents = match std::fs::read_to_string(self.layout.cache_dir.join(PROGRESS_FILE)) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        contents
            .lines()
            .rev()
            .find_map(|line| serde_json::from_str(line).ok())
            .map(Some)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no valid progress record"))
    }

    fn write_progress(&self, _lock: &File, progress: &Progress) -> io::Result<()> {
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(self.layout.cache_dir.join(PROGRESS_FILE))?;
        serde_json::to_writer(&mut file, progress).map_err(io::Error::other)?;
        writeln!(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::output::OutputMode;

    #[tokio::test(flavor = "multi_thread")]
    async fn foreground_preparation_joins_an_existing_worker() {
        let home = tempfile::tempdir().unwrap();
        let layout = WorkspaceLayout::under_root(home.path());
        let holder_state = Preparation::new(&layout);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let lock = holder_state.acquire().unwrap();
            holder_state
                .write_progress(
                    &lock,
                    &Progress {
                        pid: std::process::id(),
                        completed: 0,
                        total: 1,
                        error: None,
                    },
                )
                .unwrap();
            ready_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(250));
        });
        ready_rx.recv().unwrap();

        let started = Instant::now();
        Preparation::new(&layout)
            .wait(&Output::new(OutputMode::Human, true))
            .await
            .unwrap();
        holder.join().unwrap();

        assert!(started.elapsed() >= Duration::from_millis(200));
    }
}
