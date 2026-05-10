use crate::adapter::OmnifsExport;
use crate::error::NfsFrontendError;
use crate::server::start_server;
use omnifs_host::registry::ProviderRegistry;
use std::fs::OpenOptions;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tokio::runtime::Handle;

#[derive(Debug, Clone)]
pub struct NfsMountOptions {
    pub bind: SocketAddr,
    pub trace_path: Option<PathBuf>,
    pub state_dir: PathBuf,
}

impl NfsMountOptions {
    pub fn loopback(state_dir: PathBuf) -> Self {
        Self {
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            trace_path: None,
            state_dir,
        }
    }
}

pub fn mount_blocking(
    mount_point: &Path,
    registry: &Arc<ProviderRegistry>,
    rt: Handle,
    options: &NfsMountOptions,
) -> Result<(), NfsFrontendError> {
    std::fs::create_dir_all(mount_point)?;
    ensure_private_state_dir(&options.state_dir)?;
    let export = Arc::new(OmnifsExport::new(rt, Arc::clone(registry)));
    let server = start_server(export, options.bind, options.trace_path.clone())?;
    write_state(mount_point, server.addr(), &options.state_dir)?;
    mount_client(mount_point, server.addr())?;

    tracing::info!(
        mount = %mount_point.display(),
        addr = %server.addr(),
        "NFS loopback mount established"
    );

    while mount_is_active(mount_point) {
        thread::sleep(Duration::from_millis(500));
    }

    tracing::info!("NFS mount exited, shutting down providers");
    registry.shutdown_all();
    drop(server);
    Ok(())
}

pub fn unmount(mount_point: &Path) -> Result<(), NfsFrontendError> {
    let status = Command::new("umount")
        .arg(mount_point)
        .status()
        .map_err(|error| NfsFrontendError::Unmount(error.to_string()))?;
    if status.success() {
        Ok(())
    } else {
        Err(NfsFrontendError::Unmount(format!(
            "umount exited with {status}"
        )))
    }
}

fn mount_client(mount_point: &Path, addr: SocketAddr) -> Result<(), NfsFrontendError> {
    #[cfg(target_os = "macos")]
    {
        let source = format!(
            "127.0.0.1:/{}",
            crate::protocol::consts::DEFAULT_EXPORT_NAME
        );
        let options = format!(
            "vers=4,tcp,port={},sec=sys,ro,intr,nocallback,noac,nonegnamecache,retrycnt=0,timeo=5,retrans=1",
            addr.port()
        );
        let status = Command::new("sudo")
            .arg("-n")
            .arg("mount_nfs")
            .arg("-o")
            .arg(options)
            .arg(source)
            .arg(mount_point)
            .status()
            .map_err(|error| NfsFrontendError::Mount(error.to_string()))?;
        if status.success() {
            Ok(())
        } else {
            Err(NfsFrontendError::Mount(format!(
                "mount_nfs exited with {status}; run sudo -v and retry"
            )))
        }
    }

    #[cfg(target_os = "linux")]
    {
        let options = format!(
            "vers=4.0,proto=tcp,port={},ro,soft,timeo=5,retrans=1,lookupcache=none,actimeo=0",
            addr.port()
        );
        let status = Command::new("mount")
            .arg("-t")
            .arg("nfs4")
            .arg("-o")
            .arg(options)
            .arg(format!(
                "127.0.0.1:/{}",
                crate::protocol::consts::DEFAULT_EXPORT_NAME
            ))
            .arg(mount_point)
            .status()
            .map_err(|error| NfsFrontendError::Mount(error.to_string()))?;
        if status.success() {
            Ok(())
        } else {
            Err(NfsFrontendError::Mount(format!(
                "mount exited with {status}"
            )))
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (mount_point, addr);
        Err(NfsFrontendError::Mount(
            "automatic NFSv4 mount is not implemented on this platform".to_string(),
        ))
    }
}

fn mount_is_active(mount_point: &Path) -> bool {
    let Ok(output) = Command::new("mount").output() else {
        return false;
    };
    let wanted = mount_point.to_string_lossy();
    String::from_utf8_lossy(&output.stdout).lines().any(|line| {
        line.contains(&format!(" on {wanted} "))
            || line.contains(&format!(
                " on {}",
                canonical_mount_path(mount_point).display()
            ))
    })
}

fn canonical_mount_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn ensure_private_state_dir(state_dir: &Path) -> Result<(), NfsFrontendError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(state_dir)?;
        std::fs::set_permissions(state_dir, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(state_dir)?;
    }
    Ok(())
}

fn write_state(
    mount_point: &Path,
    addr: SocketAddr,
    state_dir: &Path,
) -> Result<(), NfsFrontendError> {
    ensure_private_state_dir(state_dir)?;
    let name = format!("mount-{}-{}.env", std::process::id(), addr.port());
    let path = state_dir.join(name);
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    writeln!(file, "mount_point={}", mount_point.display())?;
    writeln!(file, "addr={addr}")?;
    writeln!(file, "pid={}", std::process::id())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
