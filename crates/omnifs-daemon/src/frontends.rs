//! Filesystem frontends managed by the daemon.
//!
//! The daemon is a frontend registry: it constructs ONE [`TreeNamespace`] over
//! the shared mount registry and builds one renderer per requested frontend on
//! top of it. Every renderer subscribes to the same namespace event stream, so a
//! single invalidation fans out to all of them. Linux can serve FUSE and NFS
//! concurrently; macOS is NFS-only.
//!
//! Supervision is symmetric and matches the daemon's single-mount lifecycle: each
//! frontend blocks on its own thread until it is unmounted, and the first one to
//! exit (an error or an external unmount) takes the others down with it, so one
//! mount dying stops the daemon the same way it does with a single frontend.

use omnifs_api::{FrontendInfo, FsType};
use omnifs_engine::Namespace;
use omnifs_engine::TreeNamespace;
#[cfg(target_os = "linux")]
use omnifs_fuse::NotifierHandle;
#[cfg(target_os = "linux")]
use omnifs_fuse::mount;
#[cfg(target_os = "linux")]
use omnifs_mtab::proc_mounts;
use omnifs_nfs::NfsMountOptions;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use tokio::runtime::Handle;

use crate::app::{FrontendKind, FrontendMount};
use crate::context::DaemonContext;

/// The daemon's frontend registry: every in-process renderer over the shared
/// namespace. The namespace is built once in the startup path (post-reconcile)
/// and passed to `serve`, so the in-process renderers and the out-of-process
/// attach listeners serve the same [`TreeNamespace`].
pub(crate) struct Frontends {
    instances: Vec<Arc<Instance>>,
    /// Releases a namespace-only `serve` (no in-process renderer drives the
    /// lifecycle) when `unmount` fires on shutdown.
    stop_tx: mpsc::Sender<()>,
    stop_rx: std::sync::Mutex<Option<mpsc::Receiver<()>>>,
}

/// One requested frontend: the renderer over the shared namespace at its mount
/// point.
enum Instance {
    #[cfg(target_os = "linux")]
    Fuse(Fuse),
    Nfs(Nfs),
}

#[cfg(target_os = "linux")]
struct Fuse {
    mount_point: PathBuf,
    notifier: NotifierHandle,
}

struct Nfs {
    mount_point: PathBuf,
    options: NfsMountOptions,
}

impl Frontends {
    pub(crate) fn from_context(context: &DaemonContext) -> Self {
        let instances = context
            .frontends()
            .iter()
            .map(|frontend| Arc::new(Instance::build(frontend, context)))
            .collect();
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        Self {
            instances,
            stop_tx,
            stop_rx: std::sync::Mutex::new(Some(stop_rx)),
        }
    }

    /// Serve every in-process frontend over `namespace`, each blocking on its own
    /// thread until unmounted. Returns once ALL frontends have exited; the first
    /// exit unmounts the rest so the daemon comes down as one unit. The first
    /// error observed is returned. A namespace-only daemon (no in-process
    /// frontend) blocks until shutdown instead, so the attach-socket listeners
    /// keep serving.
    pub fn serve(&self, namespace: &Arc<TreeNamespace>, rt: &Handle) -> anyhow::Result<()> {
        if self.instances.is_empty() {
            // No in-process renderer drives the lifecycle; block until `unmount`
            // signals shutdown so the spawned attach listeners keep serving.
            if let Some(rx) = self.stop_rx.lock().expect("stop rx lock").take() {
                let _ = rx.recv();
            }
            return Ok(());
        }

        let (exit_tx, exit_rx) = mpsc::channel::<()>();
        let mut threads = Vec::with_capacity(self.instances.len());
        for instance in &self.instances {
            let instance = Arc::clone(instance);
            let namespace = Arc::clone(namespace) as Arc<dyn Namespace>;
            let rt = rt.clone();
            let exit_tx = exit_tx.clone();
            let label = instance.label();
            let thread = std::thread::Builder::new()
                .name(format!("frontend-{label}"))
                .spawn(move || {
                    let result = instance.serve_blocking(namespace, &rt);
                    // Signal the supervisor that a frontend exited; the receiver
                    // may already be gone if every frontend raced to exit.
                    let _ = exit_tx.send(());
                    result
                })
                .expect("spawn frontend thread");
            threads.push(thread);
        }
        drop(exit_tx);

        // Block until the first frontend exits (error or external unmount).
        let _ = exit_rx.recv();
        // One mount dying takes the daemon down: unmount the rest so their serve
        // loops unblock, then join every thread.
        self.unmount();

        let mut first_error = None;
        for thread in threads {
            match thread.join() {
                Ok(Ok(())) => {},
                Ok(Err(error)) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                },
                Err(panic) => {
                    if first_error.is_none() {
                        first_error = Some(anyhow::anyhow!("frontend thread panicked: {panic:?}"));
                    }
                },
            }
        }

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// The subset of requested frontends currently present in the OS mount table.
    pub fn serving(&self) -> Vec<FrontendInfo> {
        self.instances
            .iter()
            .filter_map(|instance| instance.serving())
            .collect()
    }

    /// Unmount every frontend. Best-effort per frontend; each unblocks its own
    /// serve loop. Also releases a namespace-only `serve`, which has no frontend
    /// to unmount.
    pub fn unmount(&self) {
        // Release a blocked namespace-only `serve`; the receiver may already be
        // gone if `serve` returned or was never namespace-only.
        let _ = self.stop_tx.send(());
        for instance in &self.instances {
            instance.unmount();
        }
    }

    /// Invalidate the kernel dentry for a root child across every FUSE frontend
    /// (NFS has no kernel-notify equivalent here).
    pub fn invalidate_root_child(&self, name: &str) {
        for instance in &self.instances {
            instance.invalidate_root_child(name);
        }
    }
}

impl Instance {
    fn build(frontend: &FrontendMount, context: &DaemonContext) -> Self {
        match frontend.kind {
            #[cfg(target_os = "linux")]
            FrontendKind::Fuse => Self::Fuse(Fuse {
                mount_point: frontend.mount_point.clone(),
                notifier: omnifs_fuse::new_notifier_handle(),
            }),
            #[cfg(not(target_os = "linux"))]
            FrontendKind::Fuse => {
                // `DaemonContext::resolve` rejects the FUSE frontend off Linux, so
                // it never reaches the registry there.
                unreachable!("the fuse frontend is only available on Linux")
            },
            FrontendKind::Nfs => Self::Nfs(Nfs {
                mount_point: frontend.mount_point.clone(),
                options: context.nfs_mount_options(),
            }),
        }
    }

    /// Block serving this frontend over the shared namespace until it is
    /// unmounted. Provider teardown is the daemon's job after `serve` returns.
    fn serve_blocking(&self, namespace: Arc<dyn Namespace>, rt: &Handle) -> anyhow::Result<()> {
        match self {
            #[cfg(target_os = "linux")]
            Instance::Fuse(frontend) => {
                mount::run_blocking(&frontend.mount_point, namespace, rt, &frontend.notifier)?;
            },
            Instance::Nfs(frontend) => {
                omnifs_nfs::mount_blocking(
                    &frontend.mount_point,
                    namespace,
                    rt.clone(),
                    &frontend.options,
                )?;
            },
        }
        Ok(())
    }

    fn serving(&self) -> Option<FrontendInfo> {
        match self {
            #[cfg(target_os = "linux")]
            Instance::Fuse(frontend) => proc_mounts::find_mount(&frontend.mount_point)
                .filter(|mount| mount.device == "omnifs" && mount.fs_type.starts_with("fuse"))
                .map(|mount| FrontendInfo {
                    source: mount.device,
                    fs_type: FsType::Fuse,
                }),
            Instance::Nfs(frontend) => nfs_serving(&frontend.mount_point),
        }
    }

    /// Unmount this frontend from within the daemon, unblocking its `serve`
    /// loop. Best-effort: a failure is logged, since `omnifs down` falls back to
    /// an external sweep.
    fn unmount(&self) {
        let result = match self {
            #[cfg(target_os = "linux")]
            Instance::Fuse(frontend) => {
                omnifs_fuse::mount::unmount(&frontend.mount_point).map_err(|e| e.to_string())
            },
            Instance::Nfs(frontend) => {
                omnifs_nfs::unmount(&frontend.mount_point).map_err(|e| e.to_string())
            },
        };
        if let Err(error) = result {
            tracing::warn!(%error, "self-unmount failed");
        }
    }

    fn invalidate_root_child(&self, name: &str) {
        match self {
            #[cfg(target_os = "linux")]
            Instance::Fuse(frontend) => {
                omnifs_fuse::invalidate_root_child(&frontend.notifier, name);
            },
            Instance::Nfs(_) => {
                let _ = name;
            },
        }
    }

    /// A short kind label for the frontend thread name.
    fn label(&self) -> &'static str {
        match self {
            #[cfg(target_os = "linux")]
            Instance::Fuse(_) => "fuse",
            Instance::Nfs(_) => "nfs",
        }
    }
}

#[cfg(target_os = "linux")]
fn nfs_serving(mount_point: &Path) -> Option<FrontendInfo> {
    proc_mounts::find_mount(mount_point)
        .filter(|mount| mount.fs_type.starts_with("nfs"))
        .map(|mount| FrontendInfo {
            source: mount.device,
            fs_type: FsType::Nfs,
        })
}

// macOS (and any host without `/proc/mounts`) reads the live OS mount table
// through omnifs-nfs, so host-native NFS readiness works off Linux.
#[cfg(not(target_os = "linux"))]
fn nfs_serving(mount_point: &Path) -> Option<FrontendInfo> {
    omnifs_nfs::mount_is_active(mount_point).then(|| FrontendInfo {
        source: "omnifs".to_string(),
        fs_type: FsType::Nfs,
    })
}
