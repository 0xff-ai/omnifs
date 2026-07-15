//! The daemon-owned record written at `<config_dir>/daemon.json`.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::io::write_atomic;
use crate::mounts::Revision;

/// Schema version this build understands. Older records are rejected outright.
pub const DAEMON_RECORD_VERSION: u32 = 4;

/// How a client reaches the daemon's control socket.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum Endpoint {
    /// Host-native daemon serving a Unix domain socket.
    Unix { path: PathBuf },
}

/// One token-authenticated namespace attach target owned by the daemon.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "lowercase", deny_unknown_fields)]
pub enum AttachRecord {
    Tcp { addr: String, token: String },
    Vsock { socket_path: PathBuf, token: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachTransport {
    Tcp,
    Vsock,
}

impl AttachRecord {
    #[must_use]
    pub const fn transport(&self) -> AttachTransport {
        match self {
            Self::Tcp { .. } => AttachTransport::Tcp,
            Self::Vsock { .. } => AttachTransport::Vsock,
        }
    }
}

/// Filesystem protocol served by a host runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FrontendKind {
    Fuse,
    Nfs,
}

impl FrontendKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Fuse => "fuse",
            Self::Nfs => "nfs",
        }
    }
}

/// The strict current daemon record stored at `<config_dir>/daemon.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonRecord {
    pub version: u32,
    pub mount_revision: Revision,
    pub endpoint: Endpoint,
    pub pid: u32,
    pub instance_id: String,
    /// RFC3339 UTC timestamp of when the daemon started serving.
    pub started_at: String,
    /// Token-authenticated TCP and vsock attach targets bound this start.
    pub attach: Vec<AttachRecord>,
}

impl DaemonRecord {
    #[must_use]
    pub fn new(
        mount_revision: Revision,
        endpoint: Endpoint,
        pid: u32,
        instance_id: String,
    ) -> Self {
        let started_at = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_default();
        Self {
            version: DAEMON_RECORD_VERSION,
            mount_revision,
            endpoint,
            pid,
            instance_id,
            started_at,
            attach: Vec::new(),
        }
    }

    /// Atomically write the record with mode `0600`.
    pub fn write(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(self)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        write_atomic(path, &json, 0o600)
    }

    /// Replace the target for one transport and keep transport order stable.
    pub fn set_attach(&mut self, target: AttachRecord) {
        let transport = target.transport();
        self.attach
            .retain(|existing| existing.transport() != transport);
        self.attach.push(target);
        self.attach.sort();
    }

    pub fn remove_attach(&mut self, target: &AttachRecord) {
        self.attach.retain(|existing| existing != target);
    }

    /// Read the record, rejecting unknown versions and fields.
    pub fn read(path: &Path) -> io::Result<Option<Self>> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("parse daemon record {}: {error}", path.display()),
            )
        })?;
        let version = value.get("version").and_then(serde_json::Value::as_u64);
        if version != Some(u64::from(DAEMON_RECORD_VERSION)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "daemon record at {} has version {}; this build understands only version {}. \
                     Run `omnifs down` with the build that started the daemon, or delete {} manually.",
                    path.display(),
                    version.map_or_else(|| "missing".to_string(), |version| version.to_string()),
                    DAEMON_RECORD_VERSION,
                    path.display(),
                ),
            ));
        }
        serde_json::from_value(value).map(Some).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("parse daemon record {}: {error}", path.display()),
            )
        })
    }

    /// Remove the record. A missing file is not an error.
    pub fn remove(path: &Path) -> io::Result<()> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

pub const DAEMON_RECORD_FILE: &str = "daemon.json";
pub const CONTROL_SOCKET_FILE: &str = "control.sock";

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> DaemonRecord {
        DaemonRecord::new(
            Revision::new("0123456789abcdef0123456789abcdef01234567").unwrap(),
            Endpoint::Unix {
                path: PathBuf::from("/home/u/.omnifs/control.sock"),
            },
            4321,
            "b1946ac92492d234".to_string(),
        )
    }

    #[test]
    fn round_trips_with_direct_pid_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DAEMON_RECORD_FILE);
        let record = sample();
        record.write(&path).unwrap();
        assert_eq!(DaemonRecord::read(&path).unwrap().unwrap(), record);
        assert_eq!(serde_json::to_value(record).unwrap()["pid"], 4321);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn attach_is_always_present_and_sorted() {
        let mut record = sample();
        record.set_attach(AttachRecord::Vsock {
            socket_path: PathBuf::from("/vsock.sock"),
            token: "b".repeat(32),
        });
        record.set_attach(AttachRecord::Tcp {
            addr: "127.0.0.1:1".to_string(),
            token: "a".repeat(32),
        });
        assert!(matches!(record.attach[0], AttachRecord::Tcp { .. }));
        assert!(
            serde_json::to_value(record)
                .unwrap()
                .get("attach")
                .is_some()
        );
    }

    #[test]
    fn unknown_version_and_fields_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DAEMON_RECORD_FILE);
        let mut value = serde_json::to_value(sample()).unwrap();
        value["version"] = 99.into();
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(DaemonRecord::read(&path).is_err());
        value["version"] = DAEMON_RECORD_VERSION.into();
        value["obsolete"] = true.into();
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(DaemonRecord::read(&path).is_err());
    }
}
