//! The daemon-owned runtime record written at `<config_dir>/daemon.json`.
//!
//! One artifact, one lifecycle: a host-native daemon writes this the moment it
//! has bound its control socket and installed its routes, and removes it on a
//! graceful exit. The Docker launcher writes it host-side once the in-container
//! daemon is serving (the container home is a bind mount and a UDS on a macOS
//! Docker bind mount is unreliable, so the container daemon speaks TCP and does
//! not own the record). It replaces both the old `launch.json` and the
//! `control-token` file: the endpoint the CLI dials, the backend identity
//! teardown needs, and the bearer token for the TCP transport all live here.
//!
//! The CLI only ever dials an endpoint it read from this record (or from
//! `OMNIFS_DAEMON_ADDR`), so a foreign daemon in another workspace is
//! structurally unaddressable.
//!
//! An unknown `version` field is reported and treated as an error rather than
//! silently ignored, matching the NFS mount-state version discipline.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::io::write_atomic;

/// Schema version this build understands. A record carrying a different version
/// was written by a build that knows something this one does not, and is
/// reported rather than silently reinterpreted.
pub const RUNTIME_RECORD_VERSION: u32 = 1;

/// How a client reaches the daemon's control API.
///
/// The `tcp` variant carries the bearer token, which is why the whole record is
/// written mode `0600`: filesystem permissions are the only thing keeping the
/// token off other users on a shared host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Endpoint {
    /// Host-native daemon serving a Unix domain socket. Auth is filesystem
    /// permissions on the socket, so there is no token here.
    Unix { path: PathBuf },
    /// Docker (or `OMNIFS_DAEMON_ADDR`) daemon serving TCP loopback. The bearer
    /// token authenticates every request but `GET /v1/ready`.
    Tcp { addr: String, token: String },
}

/// The backend serving the daemon, mirroring `omnifs_api::DaemonBackend` but
/// owned here so the workspace crate does not depend on the control-API crate.
/// `logs`, `shell`, and teardown read the container identity from the Docker
/// variant; the native variant carries the pid for a liveness-checked sweep.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "backend", rename_all = "lowercase")]
pub enum RecordedBackend {
    Native {
        pid: u32,
    },
    Docker {
        container_name: String,
        image: String,
    },
}

/// One serving frontend and where it is mounted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrontendRecord {
    pub kind: FrontendKind,
    pub mount_point: PathBuf,
}

/// Frontend protocol, owned here so the record does not depend on the daemon or
/// API crates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FrontendKind {
    Fuse,
    Nfs,
}

/// The persisted runtime record. Stored as JSON at `<config_dir>/daemon.json`.
///
/// `deny_unknown_fields` cannot be combined with the flattened `backend`
/// (serde rejects the pairing), so forward-compatibility rests on the explicit
/// version check in [`RuntimeRecord::read`] instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeRecord {
    pub version: u32,
    pub endpoint: Endpoint,
    #[serde(flatten)]
    pub backend: RecordedBackend,
    pub instance_id: String,
    pub frontends: Vec<FrontendRecord>,
    /// RFC3339 UTC timestamp of when the daemon started serving.
    pub started_at: String,
}

impl RuntimeRecord {
    /// Assemble a record stamped with the current schema version and an
    /// `started_at` of now.
    #[must_use]
    pub fn new(
        endpoint: Endpoint,
        backend: RecordedBackend,
        instance_id: String,
        frontends: Vec<FrontendRecord>,
    ) -> Self {
        let started_at = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_default();
        Self {
            version: RUNTIME_RECORD_VERSION,
            endpoint,
            backend,
            instance_id,
            frontends,
            started_at,
        }
    }

    /// Atomically write to `path` mode `0600` (the token in the tcp variant must
    /// not be world-readable). Creates the parent directory if needed.
    pub fn write(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(self)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        write_atomic(path, &json, 0o600)
    }

    /// Read the record at `path`. Returns `Ok(None)` when the file does not
    /// exist (no daemon is running). Returns an error when the file is present
    /// but unreadable, unparseable, or carries a version this build does not
    /// understand.
    pub fn read(path: &Path) -> io::Result<Option<Self>> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let record: Self = serde_json::from_slice(&bytes).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("parse runtime record {}: {error}", path.display()),
            )
        })?;
        if record.version != RUNTIME_RECORD_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "runtime record at {} has version {}; this build understands only version {}. \
                     Run `omnifs down` with the build that started the daemon, or delete {} manually.",
                    path.display(),
                    record.version,
                    RUNTIME_RECORD_VERSION,
                    path.display(),
                ),
            ));
        }
        Ok(Some(record))
    }

    /// Remove the record at `path`. A missing file is not an error.
    pub fn remove(path: &Path) -> io::Result<()> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// The container name when the daemon runs under Docker, whose mount lives
    /// inside the container. `None` for the host-native backend.
    #[must_use]
    pub fn container_name(&self) -> Option<&str> {
        match &self.backend {
            RecordedBackend::Docker { container_name, .. } => Some(container_name),
            RecordedBackend::Native { .. } => None,
        }
    }

    /// The mount point of the first serving frontend, if any.
    #[must_use]
    pub fn mount_point(&self) -> Option<&Path> {
        self.frontends
            .first()
            .map(|frontend| frontend.mount_point.as_path())
    }
}

/// The file name of the runtime record under the config directory.
pub const RUNTIME_RECORD_FILE: &str = "daemon.json";

/// The file name of the host-native control socket under the config directory.
pub const CONTROL_SOCKET_FILE: &str = "control.sock";

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_native() -> RuntimeRecord {
        RuntimeRecord::new(
            Endpoint::Unix {
                path: PathBuf::from("/home/u/.omnifs/control.sock"),
            },
            RecordedBackend::Native { pid: 4321 },
            "b1946ac92492d234".to_string(),
            vec![FrontendRecord {
                kind: FrontendKind::Nfs,
                mount_point: PathBuf::from("/home/u/omnifs"),
            }],
        )
    }

    #[test]
    fn native_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RUNTIME_RECORD_FILE);
        let record = sample_native();
        record.write(&path).unwrap();

        let read = RuntimeRecord::read(&path).unwrap().unwrap();
        assert_eq!(read, record);
        assert_eq!(read.container_name(), None);
        assert_eq!(read.mount_point(), Some(Path::new("/home/u/omnifs")));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn native_serializes_flat_backend_tag() {
        let json = serde_json::to_value(sample_native()).unwrap();
        assert_eq!(json["backend"], "native");
        assert_eq!(json["pid"], 4321);
        assert_eq!(json["endpoint"]["kind"], "unix");
        assert_eq!(json["frontends"][0]["kind"], "nfs");
    }

    #[test]
    fn docker_round_trips_and_reports_container() {
        let record = RuntimeRecord::new(
            Endpoint::Tcp {
                addr: "127.0.0.1:7878".to_string(),
                token: "deadbeef".to_string(),
            },
            RecordedBackend::Docker {
                container_name: "omnifs".to_string(),
                image: "ghcr.io/0xff-ai/omnifs:0.2.1".to_string(),
            },
            "aaaa1111bbbb2222".to_string(),
            vec![FrontendRecord {
                kind: FrontendKind::Fuse,
                mount_point: PathBuf::from("/omnifs"),
            }],
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RUNTIME_RECORD_FILE);
        record.write(&path).unwrap();
        let read = RuntimeRecord::read(&path).unwrap().unwrap();
        assert_eq!(read.container_name(), Some("omnifs"));
        match read.endpoint {
            Endpoint::Tcp { token, .. } => assert_eq!(token, "deadbeef"),
            Endpoint::Unix { .. } => panic!("expected tcp endpoint"),
        }
    }

    #[test]
    fn absent_record_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RUNTIME_RECORD_FILE);
        assert!(RuntimeRecord::read(&path).unwrap().is_none());
    }

    #[test]
    fn unknown_version_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RUNTIME_RECORD_FILE);
        std::fs::write(
            &path,
            r#"{"version":99,"endpoint":{"kind":"unix","path":"/x"},"backend":"native","pid":1,"instance_id":"x","frontends":[],"started_at":"2026-07-07T00:00:00Z"}"#,
        )
        .unwrap();
        let error = RuntimeRecord::read(&path).unwrap_err();
        assert!(error.to_string().contains("version 99"));
    }

    #[test]
    fn remove_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RUNTIME_RECORD_FILE);
        RuntimeRecord::remove(&path).unwrap();
        sample_native().write(&path).unwrap();
        RuntimeRecord::remove(&path).unwrap();
        assert!(!path.exists());
    }
}
