//! Detached provider preparation and foreground joining for daemon startup.

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use clap::Args;
use futures_util::StreamExt as _;
use omnifs_workspace::ids::ProviderId;
use omnifs_workspace::layout::{WorkspaceLayout, wasm_cache_dir};
use omnifs_workspace::provider::preparation::{Lease, Preparation, Record};
use omnifs_workspace::provider::{IndexEntry, ProviderStore};

use crate::process::ProcessRole;
use crate::ui::output::Output;

const LOCK_POLL: Duration = Duration::from_millis(100);
const CHILD_READY_POLL: Duration = Duration::from_millis(10);
const CHILD_READY_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PARALLELISM: usize = 4;

#[derive(Args, Debug, Clone, Default)]
pub(crate) struct PrepareProvidersArgs {}

impl PrepareProvidersArgs {
    pub(crate) async fn run(self) -> Result<()> {
        let layout = WorkspaceLayout::resolve()?;
        Manager::new(&layout).run_background().await
    }
}

/// Command-side owner of provider preparation for one resolved workspace.
pub(crate) struct Manager {
    layout: WorkspaceLayout,
    preparation: Preparation,
}

impl Manager {
    pub(crate) fn new(layout: &WorkspaceLayout) -> Self {
        Self {
            layout: layout.clone(),
            preparation: Preparation::new(&layout.cache_dir),
        }
    }

    /// Start a compiler process that survives this foreground command.
    pub(crate) fn spawn_background(&self, output: &Output) -> Result<()> {
        if self.preparation.is_active()? {
            output.narrate(
                "Provider runtime preparation is already running; `omnifs up` will join it.",
            );
            return Ok(());
        }
        let binary = std::env::current_exe().context("resolve the omnifs executable")?;
        std::fs::create_dir_all(&self.layout.cache_dir).with_context(|| {
            format!("create cache directory {}", self.layout.cache_dir.display())
        })?;
        let log_path = self.preparation.log_path();
        let mut log_options = OpenOptions::new();
        log_options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            log_options.mode(0o600);
        }
        let log = log_options
            .open(&log_path)
            .with_context(|| format!("open provider preparation log {}", log_path.display()))?;
        let log_err = log
            .try_clone()
            .with_context(|| format!("clone provider preparation log {}", log_path.display()))?;
        let mut command = std::process::Command::new(&binary);
        command
            .arg("prepare-providers")
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .env("OMNIFS_HOME", &self.layout.config_dir);
        if std::env::var_os("RUST_LOG").is_none() {
            command.env(
                "RUST_LOG",
                ProcessRole::ProviderPreparation.default_log_level(),
            );
        }
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
            let child_owns_record = self
                .preparation
                .read()
                .ok()
                .flatten()
                .is_some_and(|record| record.pid == child_pid);
            if child_owns_record && self.preparation.is_active()? {
                break;
            }
            if let Some(status) = child
                .try_wait()
                .context("observe provider preparation startup")?
            {
                if status.success() {
                    break;
                }
                bail!(
                    "provider preparation exited before becoming ready ({status}); see {}",
                    log_path.display()
                );
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!(
                    "provider preparation did not become ready within {}s; see {}",
                    CHILD_READY_TIMEOUT.as_secs(),
                    log_path.display()
                );
            }
            std::thread::sleep(CHILD_READY_POLL);
        }
        output.narrate(
            "Preparing the provider runtime in the background; check `omnifs status` for progress.",
        );
        Ok(())
    }

    /// Join any detached preparation, then synchronously verify the exact
    /// provider set needed by the new daemon revision.
    pub(crate) async fn prepare_for_up(
        &self,
        ids: impl IntoIterator<Item = ProviderId>,
        output: &Output,
    ) -> Result<()> {
        let ids = ids.into_iter().collect::<HashSet<_>>();
        if ids.is_empty() {
            return Ok(());
        }
        let entries = self.entries(Some(&ids))?;
        let mut progress = output.progress("providers");
        let lease = loop {
            if let Some(lease) = self.preparation.try_acquire()? {
                break lease;
            }
            let detail = self.preparation.read().ok().flatten().map_or_else(
                || "waiting for background preparation".to_owned(),
                |record| {
                    format!(
                        "waiting for background preparation ({}/{})",
                        record.completed(),
                        record.providers.len()
                    )
                },
            );
            progress.update(&detail);
            tokio::time::sleep(LOCK_POLL).await;
        };
        let result = self
            .prepare(&lease, entries, |completed, total| {
                progress.update(&format!("preparing {completed}/{total}"));
            })
            .await;
        match result {
            Ok(count) => {
                progress.settle_ok(format!("{count} ready"));
                Ok(())
            },
            Err(error) => {
                progress.settle_fail("preparation failed");
                Err(error)
            },
        }
    }

    async fn run_background(&self) -> Result<()> {
        let lease = tokio::task::spawn_blocking({
            let preparation = self.preparation.clone();
            move || preparation.acquire()
        })
        .await
        .context("join provider preparation lock task")??;
        let entries = self.entries(None)?;
        if entries.is_empty() {
            return Ok(());
        }
        self.prepare(&lease, entries, |_, _| {}).await.map(|_| ())
    }

    fn entries(&self, selected: Option<&HashSet<ProviderId>>) -> Result<Vec<IndexEntry>> {
        let index = ProviderStore::new(&self.layout.providers_dir).read_index()?;
        let entries = index
            .providers
            .into_iter()
            .filter(|entry| selected.is_none_or(|ids| ids.contains(&entry.id)))
            .collect::<Vec<_>>();
        if let Some(selected) = selected
            && entries.len() != selected.len()
        {
            let found = entries.iter().map(|entry| entry.id).collect::<HashSet<_>>();
            let missing = selected
                .difference(&found)
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            bail!(
                "provider preparation cannot find retained artifact(s): {}",
                missing.join(", ")
            );
        }
        Ok(entries)
    }

    async fn prepare(
        &self,
        lease: &Lease,
        entries: Vec<IndexEntry>,
        mut progress: impl FnMut(usize, usize),
    ) -> Result<usize> {
        let mut record = Record::running(entries.clone());
        lease.write(&record)?;
        let compiler = match omnifs_engine::ComponentCompiler::new(
            &wasm_cache_dir(&self.layout.cache_dir),
            &self.layout.providers_dir,
        ) {
            Ok(compiler) => compiler,
            Err(error) => {
                let message = format!("initialize provider compiler: {error:#}");
                for entry in &entries {
                    record.settle(entry.id, 0, Some(message.clone()));
                }
                lease.write(&record)?;
                return Err(anyhow::anyhow!(message));
            },
        };
        let total = entries.len();
        let parallelism = std::thread::available_parallelism()
            .map_or(1, std::num::NonZeroUsize::get)
            .min(MAX_PARALLELISM)
            .min(total.max(1));
        let jobs = futures_util::stream::iter(entries.into_iter().map(|entry| {
            let compiler = compiler.clone();
            async move {
                let id = entry.id;
                let started = Instant::now();
                let result = tokio::task::spawn_blocking(move || compiler.prepare(&id)).await;
                let error = match result {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(format!("{error:#}")),
                    Err(error) => Some(format!("provider compiler task failed: {error}")),
                };
                let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                (id, duration_ms, error)
            }
        }))
        .buffer_unordered(parallelism);
        tokio::pin!(jobs);
        let mut failures = Vec::new();
        while let Some((id, duration_ms, error)) = jobs.next().await {
            if let Some(error) = &error {
                failures.push(format!("{id}: {error}"));
            }
            record.settle(id, duration_ms, error);
            lease.write(&record)?;
            progress(record.completed(), total);
        }
        if failures.is_empty() {
            Ok(total)
        } else {
            bail!(
                "failed to prepare {} provider(s): {}",
                failures.len(),
                failures.join("; ")
            )
        }
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
        std::fs::create_dir_all(&layout.providers_dir).unwrap();
        let artifact = omnifs_workspace::provider::Artifact::from_file(
            omnifs_itest::provider_wasm_path("test_provider.wasm"),
        )
        .unwrap();
        let id = artifact.id();
        ProviderStore::new(&layout.providers_dir)
            .retain(&artifact)
            .unwrap();
        let preparation = Preparation::new(&layout.cache_dir);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _lease = preparation.acquire().unwrap();
            ready_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(250));
        });
        ready_rx.recv().unwrap();

        let started = Instant::now();
        Manager::new(&layout)
            .prepare_for_up([id], &Output::new(OutputMode::Human, true))
            .await
            .unwrap();
        holder.join().unwrap();

        assert!(started.elapsed() >= Duration::from_millis(200));
        let record = Preparation::new(&layout.cache_dir).read().unwrap().unwrap();
        assert_eq!(record.completed(), 1);
        assert_eq!(record.failed(), 0);
    }
}
