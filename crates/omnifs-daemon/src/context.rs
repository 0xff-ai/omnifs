//! Daemon-owned startup and control-plane context.

use anyhow::Context as _;

use omnifs_api::DaemonInfo;
use omnifs_bootstrap::{Bootstrap, Daemon, Instance};
use std::net::SocketAddr;
use std::num::NonZeroU16;
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;

/// Attach TCP endpoints are searched starting here, offset by a per-profile
/// digest so unrelated profiles on one host do not collide on a first guess.
pub(crate) const ATTACH_PORT_MIN: u16 = 20_000;
pub(crate) const ATTACH_PORT_COUNT: u16 = 10_000;

pub(crate) struct DaemonContext {
    endpoint: Bootstrap<Daemon>,
    attach_socket: PathBuf,
    /// Random per-start id reported in status and written to process identity.
    instance_id: String,
    daemon_instance: [u8; 16],
    process: Instance,
}

impl DaemonContext {
    pub(crate) fn resolve() -> anyhow::Result<Self> {
        let endpoint = Bootstrap::<Daemon>::for_daemon()?;
        let attach_socket = endpoint.bootstrap_dir().join("daemon-state/local.sock");
        let process = Instance::current()?;
        let mut daemon_instance = [0_u8; 16];
        hex::decode_to_slice(process.instance_token(), &mut daemon_instance)
            .context("decode daemon process instance token")?;

        Ok(Self {
            endpoint,
            attach_socket,
            instance_id: process.instance_token().to_owned(),
            daemon_instance,
            process,
        })
    }

    pub(crate) fn prepare_startup_dirs(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(self.endpoint.bootstrap_dir())?;
        if let Some(parent) = self.attach_socket.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(())
    }

    pub(crate) fn control_socket(&self) -> PathBuf {
        self.endpoint.control_socket()
    }

    pub(crate) fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub(crate) fn namespace_epoch(&self) -> omnifs_vfs::NamespaceEpoch {
        omnifs_vfs::NamespaceEpoch::initial(self.daemon_instance)
    }

    pub(crate) fn attach_socket(&self) -> PathBuf {
        self.attach_socket.clone()
    }

    pub(crate) fn daemon_state_root(&self) -> PathBuf {
        self.endpoint.bootstrap_dir().join("daemon-state")
    }

    /// Bind the host-native control socket at `<profile>/control.sock`.
    pub(crate) fn bind_control_socket(&self) -> anyhow::Result<UnixListener> {
        self.endpoint
            .bind_control_socket()
            .with_context(|| format!("bind control socket {}", self.control_socket().display()))
    }

    pub(crate) fn endpoint(&self) -> &Bootstrap<Daemon> {
        &self.endpoint
    }

    pub(crate) fn process_identity(&self) -> &Instance {
        &self.process
    }

    pub(crate) fn daemon_info(
        &self,
        attach_unix: Option<PathBuf>,
        attach_tcp: Option<SocketAddr>,
    ) -> DaemonInfo {
        DaemonInfo {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            pid: self.process.pid(),
            instance_id: self.instance_id.clone(),
            executable: self.process.executable().to_path_buf(),
            attach_unix,
            attach_tcp,
        }
    }

    /// Deterministic per-profile search order over the attach TCP port
    /// range, so a fresh daemon and a restarted one land on the same first
    /// guess before falling back to `StateStore`'s persisted port.
    pub(crate) fn attach_port_candidates(&self) -> impl Iterator<Item = NonZeroU16> + use<> {
        let digest = blake3::hash(self.endpoint.bootstrap_dir().as_os_str().as_encoded_bytes());
        let bytes = digest.as_bytes();
        let offset = u16::from_le_bytes([bytes[0], bytes[1]]) % ATTACH_PORT_COUNT;
        (0..ATTACH_PORT_COUNT).map(move |step| {
            let port = ATTACH_PORT_MIN + ((offset + step) % ATTACH_PORT_COUNT);
            NonZeroU16::new(port).expect("attach port range excludes zero")
        })
    }

    /// Reject a control connection from a peer that does not own the
    /// control socket, the sole authentication this local protocol relies on.
    pub(crate) fn verify_control_peer(
        &self,
        stream: &tokio::net::UnixStream,
    ) -> anyhow::Result<()> {
        let peer = stream
            .peer_cred()
            .context("read control peer credentials")?;
        let socket = self.control_socket();
        let owner = std::fs::metadata(&socket)
            .with_context(|| format!("read control socket metadata {}", socket.display()))?
            .uid();
        anyhow::ensure!(
            peer.uid() == owner,
            "control peer uid {} does not match socket owner {owner}",
            peer.uid()
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::path::Path;
    use tempfile::TempDir;

    fn context(root: &Path) -> DaemonContext {
        let endpoint = Bootstrap::<Daemon>::under_root(root);
        let process = Instance::current().unwrap();
        DaemonContext {
            attach_socket: root.join("daemon-state/local.sock"),
            endpoint,
            instance_id: "test-instance".to_owned(),
            daemon_instance: [0x42; 16],
            process,
        }
    }

    #[test]
    fn prepare_control_path_replaces_reserved_regular_file() {
        let temp = TempDir::new().unwrap();
        let daemon = context(temp.path());
        daemon.prepare_startup_dirs().unwrap();
        let path = daemon.control_socket();
        std::fs::write(&path, b"reserved").unwrap();

        let listener = daemon.bind_control_socket().unwrap();
        drop(listener);
        assert!(path.exists());
    }

    #[test]
    fn prepare_control_path_unlinks_symlink_without_touching_target() {
        let temp = TempDir::new().unwrap();
        let daemon = context(temp.path());
        daemon.prepare_startup_dirs().unwrap();
        let target = temp.path().join("target");
        std::fs::write(&target, b"keep").unwrap();
        let path = daemon.control_socket();
        symlink(&target, &path).unwrap();

        let listener = daemon.bind_control_socket().unwrap();
        drop(listener);
        assert!(target.exists(), "symlink target must not be removed");
        assert!(path.exists());
    }
}
