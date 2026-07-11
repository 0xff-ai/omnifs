//! Daemon and frontend shutdown workflows.

use crate::workspace::Workspace;
use omnifs_workspace::runtime_record::{RecordedBackend, RuntimeRecord};

pub(crate) struct DaemonTeardown<'a> {
    workspace: &'a Workspace,
}

impl<'a> DaemonTeardown<'a> {
    pub(crate) fn new(workspace: &'a Workspace) -> Self {
        Self { workspace }
    }

    /// Stop frontends before stopping the namespace daemon they depend on.
    pub(crate) async fn down(&self, force: bool) -> anyhow::Result<()> {
        crate::commands::frontend::down::teardown(self.workspace.layout(), force).await?;

        let record_path = self.workspace.layout().runtime_record_file();
        match self.workspace.daemon().status_optional().await {
            Ok(Some(status)) => {
                anstream::println!("Stopping daemon (pid {})...", status.pid);
                match self.workspace.daemon().shutdown().await? {
                    Some(_) => anstream::println!("✓ Daemon stopped"),
                    None => anstream::println!("Daemon exited before shutdown completed."),
                }
                RuntimeRecord::remove(&record_path)?;
            },
            Ok(None) => self.remove_stale_record()?,
            Err(error) => match self.recorded_pid_liveness()? {
                Some(true) => anyhow::bail!(
                    "daemon status failed while the recorded process is still alive; \
                         ownership cannot be verified, so the process was not signalled and \
                         daemon.json was kept. Stop it manually, then retry: {error:#}"
                ),
                Some(false) => self.remove_stale_record()?,
                None => return Err(error),
            },
        }
        Ok(())
    }

    /// Best-effort daemon teardown for `omnifs reset`.
    pub(crate) async fn reset_best_effort(&self) {
        self.teardown_frontends().await;

        match self.workspace.daemon().status_optional().await {
            Ok(Some(_)) => match self.workspace.daemon().shutdown().await {
                Ok(Some(_)) => anstream::println!("✓ Daemon stopped"),
                Ok(None) => anstream::println!("No daemon answered shutdown."),
                Err(error) => {
                    anstream::eprintln!("⚠  Daemon shutdown call failed: {error:#}");
                },
            },
            Ok(None) => {
                if let Err(error) = self.remove_stale_record() {
                    anstream::eprintln!("⚠  Stale runtime record kept: {error:#}");
                }
            },
            Err(error) => {
                anstream::eprintln!("⚠  Could not verify daemon ownership: {error:#}");
            },
        }
    }

    async fn teardown_frontends(&self) {
        if let Err(error) =
            crate::commands::frontend::down::teardown(self.workspace.layout(), false).await
        {
            anstream::eprintln!("⚠  Frontend teardown failed: {error:#}");
        }
    }

    fn remove_stale_record(&self) -> anyhow::Result<()> {
        let path = self.workspace.layout().runtime_record_file();
        match self.recorded_pid_liveness()? {
            Some(true) => anyhow::bail!(
                "the daemon did not answer, but its recorded process is still alive; ownership \
                 cannot be verified, so the process was not signalled and daemon.json was kept"
            ),
            Some(false) => {
                anstream::println!("No live daemon found; removing stale runtime record...");
                RuntimeRecord::remove(&path)?;
            },
            None => anstream::println!("Nothing to tear down."),
        }
        Ok(())
    }

    fn recorded_pid_liveness(&self) -> anyhow::Result<Option<bool>> {
        let Some(record) = RuntimeRecord::read(&self.workspace.layout().runtime_record_file())?
        else {
            return Ok(None);
        };
        let RecordedBackend::Native { pid } = record.backend;
        Ok(Some(crate::process::is_alive(pid)))
    }
}
