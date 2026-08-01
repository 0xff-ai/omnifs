//! Minimal daemon bootstrap used by commands that need the control plane.

use anyhow::{Context as _, ensure};
use omnifs_api::DaemonPhase;
use omnifs_bootstrap::Profile;
use std::process::Stdio;
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::process::{Child, Command};

use crate::error::{ExitCode, WithExitCode as _};
use crate::rpc::RpcClient;
use crate::ui::output::Output;

const READY_TIMEOUT: Duration = Duration::from_mins(2);
const READY_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Every mutating and daemon-dependent command routes through here first.
/// Readiness polling can take up to `READY_TIMEOUT`, so a spawn narrates
/// exactly one line before it starts waiting; an already-running daemon
/// narrates nothing, since there is nothing this command is doing that the
/// operator does not already know about.
pub(crate) async fn start(output: &Output) -> anyhow::Result<()> {
    let endpoint = Profile::resolve()?;
    let _spawn_lock = endpoint
        .acquire_spawn_lock()
        .context("acquire daemon spawn lock")?;
    let rpc = RpcClient::resolve()?;
    let mut child = if control_reachable(&endpoint).await? {
        None
    } else {
        output.narrate("Starting the daemon");
        Some(spawn()?)
    };
    wait_until_ready(&rpc, child.as_mut()).await
}

async fn control_reachable(endpoint: &Profile) -> anyhow::Result<bool> {
    match UnixStream::connect(endpoint.control_socket()).await {
        Ok(stream) => {
            drop(stream);
            Ok(true)
        },
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(false)
        },
        Err(error) => Err(error).with_context(|| {
            format!(
                "probe daemon control socket {}",
                endpoint.control_socket().display()
            )
        }),
    }
}

fn spawn() -> anyhow::Result<Child> {
    let binary = std::env::current_exe().context("resolve the omnifs executable")?;
    let mut command = Command::new(&binary);
    command
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.process_group(0);
    command
        .spawn()
        .with_context(|| format!("spawn omnifs daemon ({})", binary.display()))
}

async fn wait_until_ready(rpc: &RpcClient, child: Option<&mut Child>) -> anyhow::Result<()> {
    // Bundled so the check closure captures nothing from the enclosing scope
    // and takes everything by explicit `&mut` argument instead: an `FnMut`
    // closure cannot itself return a future that borrows its own captured
    // environment, but a fresh reborrow of an argument can.
    struct Wait<'a> {
        rpc: &'a RpcClient,
        child: Option<&'a mut Child>,
        expected_pid: Option<u32>,
    }
    let expected_pid = child.as_ref().and_then(|child| child.id());
    let mut wait = Wait {
        rpc,
        child,
        expected_pid,
    };
    let ready =
        crate::process::poll_until_mut(READY_TIMEOUT, READY_POLL_INTERVAL, &mut wait, |wait| {
            Box::pin(async move {
                if let Some(child) = wait.child.as_mut()
                    && let Some(status) = child.try_wait().context("poll daemon child status")?
                {
                    return Err(anyhow::anyhow!(
                        "omnifs daemon exited before it became ready ({status})"
                    ))
                    .with_exit_code(ExitCode::DaemonUnavailable)
                    .map(Some);
                }

                if let Ok(inventory) = wait.rpc.inventory().await {
                    if let Some(expected_pid) = wait.expected_pid {
                        ensure!(
                            inventory.info.pid == expected_pid,
                            "daemon readiness came from pid {}, not spawned pid {expected_pid}",
                            inventory.info.pid
                        );
                    }
                    match inventory.phase {
                        DaemonPhase::Ready => {
                            wait.rpc.ready().await?;
                            return Ok(Some(()));
                        },
                        DaemonPhase::RecoveryRequired => {
                            return Err(anyhow::anyhow!(
                                "daemon requires recovery: {}",
                                inventory.health.control.message
                            ))
                            .with_exit_code(ExitCode::DaemonUnavailable)
                            .map(Some);
                        },
                        DaemonPhase::Starting => {},
                    }
                }
                Ok(None)
            })
        })
        .await?;
    if ready.is_some() {
        return Ok(());
    }
    if let Some(child) = wait.child.as_mut() {
        let _ = child.kill().await;
    }
    Err(anyhow::anyhow!(
        "omnifs daemon did not become ready within {}s",
        READY_TIMEOUT.as_secs()
    ))
    .with_exit_code(ExitCode::DaemonUnavailable)
}
