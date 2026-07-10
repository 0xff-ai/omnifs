//! The out-of-process frontend runner: `omnifs frontend run`.
//!
//! It attaches a [`WireNamespace`] to a daemon-served namespace (a Unix socket
//! for a bare-metal runner, or TCP loopback for the Docker-hosted FUSE frontend,
//! which cannot share a host Unix socket into its container) and runs the same
//! renderer entry the daemon uses (`omnifs_fuse::mount::run_blocking` /
//! `omnifs_nfs::mount_blocking`) over it, blocking until unmount. This is the
//! runtime-redesign phase-3 proof that a renderer can serve the projected tree
//! from a different process than the one that owns the projection; the Docker
//! frontend (phase 4) is its first real caller.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use clap::Args;
use omnifs_api::{OMNIFS_ATTACH_ADDR_ENV, OMNIFS_ATTACH_TOKEN_ENV};
use omnifs_engine::{Namespace, NsAttachEvent};
use omnifs_namespace_wire::{AttachTarget, WireNamespace};
use tokio::runtime::Handle;
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::app::FrontendKind;

/// Attach-event fan-out capacity. Reattach events are rare; a small ring is
/// plenty and a lagging consumer re-syncs on the next reattach.
const ATTACH_CAPACITY: usize = 16;

/// Arguments for the hidden `omnifs frontend run` subcommand.
#[derive(Args, Debug)]
pub struct FrontendArgs {
    /// Path to the daemon's namespace attach socket to connect to
    /// (`$OMNIFS_HOME/frontends/<name>.sock`). Mutually exclusive with the TCP
    /// attach path: when absent, `OMNIFS_ATTACH_ADDR`/`OMNIFS_ATTACH_TOKEN` in
    /// the environment select the TCP attach listener instead (the
    /// Docker-hosted frontend's only option, since it cannot share a host Unix
    /// socket into its container).
    #[arg(long)]
    pub attach: Option<PathBuf>,
    /// Renderer protocol to mount: `fuse` (Linux only) or `nfs`.
    #[arg(long, value_enum)]
    pub kind: FrontendKind,
    /// Host-visible mount point to serve the projected tree at.
    #[arg(long)]
    pub mount_point: PathBuf,
    /// NFS mount-state directory. Defaults under the workspace cache dir.
    #[arg(long)]
    pub nfs_state_dir: Option<PathBuf>,
    /// NFS server port to bind. A restarted runner must rebind the SAME port the
    /// kernel client is connected to, so a restartable frontend pins it here; `0`
    /// (the default) binds an ephemeral port.
    #[arg(long, default_value_t = 0)]
    pub nfs_port: u16,
}

/// Resolve the attach target: the explicit `--attach <socket>` when given,
/// otherwise the target named by `OMNIFS_ATTACH_ADDR`/`OMNIFS_ATTACH_TOKEN`
/// (the Docker frontend launcher sets both for TCP; the krunkit launcher will
/// set both for `vsock:<port>`). Neither present is a hard error: there is no
/// default to fall back to silently.
fn resolve_attach_target(attach: Option<PathBuf>) -> anyhow::Result<AttachTarget> {
    if let Some(socket) = attach {
        return Ok(AttachTarget::Unix(socket));
    }
    attach_target_from_env(
        std::env::var(OMNIFS_ATTACH_ADDR_ENV).ok(),
        std::env::var(OMNIFS_ATTACH_TOKEN_ENV).ok(),
    )
}

/// The env-driven half of [`resolve_attach_target`], pulled out as a pure
/// function of its two inputs so the parse/validation logic is unit-testable
/// without mutating process environment.
///
/// `addr` is `vsock:<port>` for the krunkit guest (there is no host name to
/// resolve: the guest always dials `VMADDR_CID_HOST`, so only the port varies)
/// or a plain `host:port` string for TCP, kept unparsed rather than a
/// pre-resolved `SocketAddr`: the Docker-hosted frontend dials
/// `host.docker.internal`, a name Docker injects into the container's DNS
/// that only resolves inside the container, so the runner cannot validate it
/// any earlier than `TcpStream::connect` does. A literal host named `vsock`
/// (vanishingly unlikely, and never how Docker names its bridge) resolves to
/// the vsock form; there is no way to address a real host by that name that
/// this grammar would rather preserve.
fn attach_target_from_env(
    addr: Option<String>,
    token: Option<String>,
) -> anyhow::Result<AttachTarget> {
    let addr = addr.with_context(|| {
        format!(
            "neither --attach nor {OMNIFS_ATTACH_ADDR_ENV} is set; the frontend runner needs one \
             attach target"
        )
    })?;
    let token = token.with_context(|| {
        format!("{OMNIFS_ATTACH_ADDR_ENV} is set but {OMNIFS_ATTACH_TOKEN_ENV} is not")
    })?;
    if let Some(port) = addr.strip_prefix("vsock:") {
        let port: u32 = port.parse().with_context(|| {
            format!("{OMNIFS_ATTACH_ADDR_ENV} `{addr}` has an invalid vsock port")
        })?;
        return Ok(AttachTarget::Vsock { port, token });
    }
    anyhow::ensure!(
        addr.rsplit_once(':')
            .is_some_and(|(_, port)| port.parse::<u16>().is_ok()),
        "{OMNIFS_ATTACH_ADDR_ENV} `{addr}` is not a `host:port` address"
    );
    Ok(AttachTarget::Tcp { addr, token })
}

/// Attach to the namespace and run the requested renderer, blocking until the
/// mount is torn down. Expects to run on a tokio runtime (the CLI's), like
/// [`run`](crate::run); it never nests a runtime.
pub fn run_frontend(args: FrontendArgs) -> anyhow::Result<()> {
    #[cfg(not(target_os = "linux"))]
    if args.kind == FrontendKind::Fuse {
        anyhow::bail!("the fuse frontend is only available on Linux");
    }

    let target = resolve_attach_target(args.attach.clone())?;
    let target_label = target.to_string();
    let rt = Handle::current();
    let namespace = attach_blocking(&rt, target)?;
    info!(
        target = %target_label,
        instance = %namespace.instance_id(),
        "attached to namespace"
    );

    // Bridge the wire attach events onto the engine-owned `NsAttachEvent` a
    // frontend acts on, so the NFS renderer re-resolves on a daemon restart
    // without omnifs-nfs depending on the wire crate. This also logs every
    // reattach and keeps the wire broadcast drained.
    let attach_tx = spawn_reattach_bridge(&rt, &namespace);

    let namespace_dyn = Arc::clone(&namespace) as Arc<dyn Namespace>;
    install_signal_handler(&rt, args.kind, args.mount_point.clone());

    match args.kind {
        #[cfg(target_os = "linux")]
        FrontendKind::Fuse => {
            let notifier = omnifs_fuse::new_notifier_handle();
            omnifs_fuse::mount::run_blocking(&args.mount_point, namespace_dyn, &rt, &notifier)?;
        },
        #[cfg(not(target_os = "linux"))]
        FrontendKind::Fuse => unreachable!("the fuse frontend is rejected off Linux above"),
        FrontendKind::Nfs => {
            let state_dir = match args.nfs_state_dir {
                Some(dir) => dir,
                None => omnifs_workspace::layout::WorkspaceLayout::resolve()?.nfs_state_dir(),
            };
            let mut options = omnifs_nfs::NfsMountOptions::loopback(state_dir);
            // The out-of-process runner is the restartable unit: persist the
            // filehandle table and pin the server port so a restart decodes the
            // handles the kernel client still holds.
            options.persist_filehandles = true;
            options.bind = std::net::SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                args.nfs_port,
            );
            omnifs_nfs::mount_blocking(
                &args.mount_point,
                namespace_dyn,
                rt.clone(),
                &options,
                Some(attach_tx.subscribe()),
            )?;
        },
    }
    info!(mount = %args.mount_point.display(), "frontend exited");
    Ok(())
}

/// Attach on the runtime, blocking this thread on the result. Mirrors the
/// daemon's blocking-thread pattern: the attach future runs on a worker while the
/// calling thread waits, so no nested runtime is created.
fn attach_blocking(rt: &Handle, target: AttachTarget) -> anyhow::Result<Arc<WireNamespace>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let rt_for_task = rt.clone();
    rt.spawn(async move {
        let _ = tx.send(WireNamespace::attach(target, rt_for_task).await);
    });
    rx.recv()
        .map_err(|_| anyhow::anyhow!("attach task dropped before completing"))?
        .map_err(|error| anyhow::anyhow!("{error}"))
}

/// Log every reattach (a reconnect that lands on a restarted daemon), keep the
/// wire attach-event broadcast drained, and forward each reattach as an engine
/// [`NsAttachEvent`] a renderer acts on. Returns the sender the renderer
/// subscribes to.
fn spawn_reattach_bridge(
    rt: &Handle,
    namespace: &Arc<WireNamespace>,
) -> broadcast::Sender<NsAttachEvent> {
    let (tx, _) = broadcast::channel(ATTACH_CAPACITY);
    let mut receiver = namespace.subscribe_attach_events();
    let forward = tx.clone();
    rt.spawn(async move {
        while let Ok(event) = receiver.recv().await {
            let omnifs_namespace_wire::AttachEvent::Reattached {
                old_instance,
                new_instance,
            } = event;
            warn!(
                %old_instance, %new_instance,
                "daemon restarted under the frontend; re-resolving cached node ids"
            );
            // A closed channel means the renderer never subscribed (a non-NFS
            // kind) or has exited; the log above is still useful.
            let _ = forward.send(NsAttachEvent::Reattached);
        }
    });
    tx
}

/// On `SIGTERM`/`SIGINT`, unmount the mount point so the blocking renderer loop
/// unblocks and the runner exits. Mirrors the daemon's signal handling.
#[cfg(unix)]
fn install_signal_handler(rt: &Handle, kind: FrontendKind, mount_point: PathBuf) {
    use tokio::signal::unix::{SignalKind, signal};

    rt.spawn(async move {
        let (mut term, mut interrupt) = match (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) {
            (Ok(term), Ok(interrupt)) => (term, interrupt),
            (term, interrupt) => {
                if let Err(error) = term {
                    warn!(%error, "failed to install SIGTERM handler");
                }
                if let Err(error) = interrupt {
                    warn!(%error, "failed to install SIGINT handler");
                }
                return;
            },
        };
        let signal = tokio::select! {
            _ = term.recv() => "SIGTERM",
            _ = interrupt.recv() => "SIGINT",
        };
        info!(signal, "received shutdown signal; unmounting frontend");
        let result = match kind {
            #[cfg(target_os = "linux")]
            FrontendKind::Fuse => {
                omnifs_fuse::mount::unmount(&mount_point).map_err(|error| error.to_string())
            },
            #[cfg(not(target_os = "linux"))]
            FrontendKind::Fuse => Ok(()),
            FrontendKind::Nfs => {
                omnifs_nfs::unmount(&mount_point).map_err(|error| error.to_string())
            },
        };
        if let Err(error) = result {
            warn!(%error, "frontend self-unmount failed");
        }
    });
}

#[cfg(not(unix))]
fn install_signal_handler(_rt: &Handle, _kind: FrontendKind, _mount_point: PathBuf) {}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn attach_prefers_explicit_unix_socket() {
        let target = resolve_attach_target(Some(PathBuf::from("/tmp/x.sock"))).unwrap();
        assert!(matches!(target, AttachTarget::Unix(path) if path == Path::new("/tmp/x.sock")));
    }

    #[test]
    fn attach_falls_back_to_tcp_env_vars() {
        let target = attach_target_from_env(
            Some("host.docker.internal:54321".to_string()),
            Some("secret".to_string()),
        )
        .unwrap();
        match target {
            AttachTarget::Tcp { addr, token } => {
                assert_eq!(addr, "host.docker.internal:54321");
                assert_eq!(token, "secret");
            },
            other => panic!("expected a tcp target, got {other:?}"),
        }
    }

    #[test]
    fn attach_env_requires_both_addr_and_token() {
        attach_target_from_env(None, None).expect_err("neither var set must fail");
        attach_target_from_env(Some("host.docker.internal:1".to_string()), None)
            .expect_err("addr without token must fail");
        attach_target_from_env(None, Some("secret".to_string()))
            .expect_err("token without addr must fail");
    }

    #[test]
    fn attach_env_rejects_a_portless_address() {
        attach_target_from_env(
            Some("host.docker.internal".to_string()),
            Some("secret".to_string()),
        )
        .expect_err("an address with no port must fail");
    }

    #[test]
    fn attach_falls_back_to_vsock_env_vars() {
        let target =
            attach_target_from_env(Some("vsock:9000".to_string()), Some("secret".to_string()))
                .unwrap();
        match target {
            AttachTarget::Vsock { port, token } => {
                assert_eq!(port, 9000);
                assert_eq!(token, "secret");
            },
            other => panic!("expected a vsock target, got {other:?}"),
        }
    }

    #[test]
    fn attach_env_rejects_vsock_with_no_port() {
        attach_target_from_env(Some("vsock:".to_string()), Some("secret".to_string()))
            .expect_err("a vsock address with no port must fail");
    }

    #[test]
    fn attach_env_rejects_vsock_with_a_bad_port() {
        attach_target_from_env(
            Some("vsock:not-a-port".to_string()),
            Some("secret".to_string()),
        )
        .expect_err("a non-numeric vsock port must fail");
        attach_target_from_env(
            Some("vsock:99999999999".to_string()),
            Some("secret".to_string()),
        )
        .expect_err("a vsock port that overflows u32 must fail");
    }

    #[test]
    fn attach_vsock_takes_precedence_over_a_host_literally_named_vsock() {
        // `vsock:8080` is ambiguous between "a host named vsock on port 8080"
        // and the vsock transport; the grammar resolves it to vsock, since
        // there is no other way to address the vsock transport at all, while a
        // host named `vsock` is a name a caller could always change.
        let target =
            attach_target_from_env(Some("vsock:8080".to_string()), Some("secret".to_string()))
                .unwrap();
        assert!(matches!(target, AttachTarget::Vsock { port: 8080, .. }));
    }
}
