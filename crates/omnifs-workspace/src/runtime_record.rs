//! The daemon-owned runtime record written at `<config_dir>/daemon.json`.
//!
//! One artifact, one lifecycle: the host-native daemon writes this the moment
//! it has bound its control socket and installed its routes, and removes it on
//! a graceful exit. The endpoint the CLI dials, backend identity teardown
//! needs, and guest attachment targets all live here.
//!
//! An unknown `version` field is reported and treated as an error rather than
//! silently ignored, matching the NFS mount-state version discipline.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::io::write_atomic;
use crate::mounts::Revision;

/// Schema version this build understands. A record carrying a different version
/// was written by a build that knows something this one does not, and is
/// reported rather than silently reinterpreted.
pub const RUNTIME_RECORD_VERSION: u32 = 3;

/// How a client reaches the daemon's control socket. The daemon always serves a
/// Unix domain socket; kept as a named type (rather than a bare path) for the
/// same reason [`RecordedBackend`] stays an enum: the on-disk schema's `kind`
/// tag is a stable wire contract, not a Rust-only convenience.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Endpoint {
    /// Host-native daemon serving a Unix domain socket. Auth is filesystem
    /// permissions on the socket, so there is no token here.
    Unix { path: PathBuf },
}

/// The backend serving the daemon, mirroring `omnifs_api::DaemonBackend` but
/// owned here so the workspace crate does not depend on the control-API crate.
/// The native variant carries the pid for a liveness-checked sweep.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "backend", rename_all = "lowercase")]
pub enum RecordedBackend {
    Native { pid: u32 },
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

/// Frontend protocol, owned here so the record does not depend on the daemon or
/// API crates.
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

/// The persisted runtime record. Stored as JSON at `<config_dir>/daemon.json`.
///
/// `deny_unknown_fields` cannot be combined with the flattened `backend`
/// (serde rejects the pairing), so forward-compatibility rests on the explicit
/// version check in [`RuntimeRecord::read`] instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeRecord {
    pub version: u32,
    pub mount_revision: Revision,
    pub endpoint: Endpoint,
    #[serde(flatten)]
    pub backend: RecordedBackend,
    pub instance_id: String,
    /// RFC3339 UTC timestamp of when the daemon started serving.
    pub started_at: String,
    /// Token-authenticated TCP and vsock attach targets bound this start.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attach: Vec<AttachRecord>,
}

impl RuntimeRecord {
    /// Assemble a record stamped with the current schema version and an
    /// `started_at` of now. Attach targets start empty and are added by the
    /// daemon's serialized record owner as listeners bind.
    #[must_use]
    pub fn new(
        mount_revision: Revision,
        endpoint: Endpoint,
        backend: RecordedBackend,
        instance_id: String,
    ) -> Self {
        let started_at = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_default();
        Self {
            version: RUNTIME_RECORD_VERSION,
            mount_revision,
            endpoint,
            backend,
            instance_id,
            started_at,
            attach: Vec::new(),
        }
    }

    /// Atomically write to `path` mode `0600` so attachment tokens cannot be
    /// exposed through the runtime record. Creates the parent directory if
    /// needed.
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
        let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("parse runtime record {}: {error}", path.display()),
            )
        })?;
        let version = value.get("version").and_then(serde_json::Value::as_u64);
        if version != Some(u64::from(RUNTIME_RECORD_VERSION)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "runtime record at {} has version {}; this build understands only version {}. \
                     Run `omnifs down` with the build that started the daemon, or delete {} manually.",
                    path.display(),
                    version.map_or_else(|| "missing".to_string(), |version| version.to_string()),
                    RUNTIME_RECORD_VERSION,
                    path.display(),
                ),
            ));
        }
        if value.get("frontends").is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "runtime record at {} contains removed live frontend state",
                    path.display()
                ),
            ));
        }
        let record: Self = serde_json::from_value(value).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("parse runtime record {}: {error}", path.display()),
            )
        })?;
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
            Revision::new("0123456789abcdef0123456789abcdef01234567").unwrap(),
            Endpoint::Unix {
                path: PathBuf::from("/home/u/.omnifs/control.sock"),
            },
            RecordedBackend::Native { pid: 4321 },
            "b1946ac92492d234".to_string(),
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
        assert!(read.attach.is_empty());

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
        assert!(json.get("frontends").is_none());
    }

    #[test]
    fn attach_is_absent_by_default_and_not_serialized() {
        let record = sample_native();
        assert!(record.attach.is_empty());
        let json = serde_json::to_value(&record).unwrap();
        assert!(
            json.get("attach").is_none(),
            "an absent attach must not serialize as a null field: {json}"
        );
    }

    #[test]
    fn attach_transports_round_trip_together() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RUNTIME_RECORD_FILE);
        let mut record = sample_native();
        record.attach = vec![
            AttachRecord::Tcp {
                addr: "127.0.0.1:54321".to_string(),
                token: "a".repeat(32),
            },
            AttachRecord::Vsock {
                socket_path: PathBuf::from("/home/u/.omnifs/frontends/vsock-attach.sock"),
                token: "b".repeat(32),
            },
        ];
        record.write(&path).unwrap();

        let read = RuntimeRecord::read(&path).unwrap().unwrap();
        assert_eq!(read.attach, record.attach);

        let json = serde_json::to_value(&record).unwrap();
        assert_eq!(json["attach"][0]["transport"], "tcp");
        assert_eq!(json["attach"][0]["addr"], "127.0.0.1:54321");
        assert_eq!(json["attach"][0]["token"], "a".repeat(32));
        assert_eq!(json["attach"][1]["transport"], "vsock");
        assert_eq!(
            json["attach"][1]["socket_path"],
            "/home/u/.omnifs/frontends/vsock-attach.sock"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the attach token must stay behind 0600 too");
        }
    }

    #[test]
    fn record_updates_are_deterministic() {
        let mut record = sample_native();
        record.set_attach(AttachRecord::Vsock {
            socket_path: PathBuf::from("/vsock.sock"),
            token: "b".repeat(32),
        });
        record.set_attach(AttachRecord::Tcp {
            addr: "127.0.0.1:1".to_string(),
            token: "a".repeat(32),
        });
        assert!(matches!(record.attach[0], AttachRecord::Tcp { .. }));
        assert!(matches!(record.attach[1], AttachRecord::Vsock { .. }));
    }

    #[test]
    fn a_record_written_without_attach_reads_back_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RUNTIME_RECORD_FILE);
        // No token-authenticated attach targets were bound.
        std::fs::write(
            &path,
            r#"{"version":3,"mount_revision":"0123456789abcdef0123456789abcdef01234567","endpoint":{"kind":"unix","path":"/x"},"backend":"native","pid":1,"instance_id":"x","started_at":"2026-07-07T00:00:00Z"}"#,
        )
        .unwrap();
        let read = RuntimeRecord::read(&path).unwrap().unwrap();
        assert!(read.attach.is_empty());
    }

    #[test]
    fn unknown_version_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RUNTIME_RECORD_FILE);
        std::fs::write(
            &path,
            r#"{"version":99,"endpoint":{"kind":"unix","path":"/x"},"backend":"native","pid":1,"instance_id":"x","started_at":"2026-07-07T00:00:00Z"}"#,
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
