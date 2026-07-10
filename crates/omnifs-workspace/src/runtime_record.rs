//! The daemon-owned runtime record written at `<config_dir>/daemon.json`.
//!
//! One artifact, one lifecycle: the host-native daemon writes this the moment
//! it has bound its control socket and installed its routes, and removes it on
//! a graceful exit. It replaces both the old `launch.json` and the
//! `control-token` file: the endpoint the CLI dials, the backend identity
//! teardown needs, and (on the debug TCP path) the bearer token all live here.
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

/// How a client reaches the daemon's control API. The daemon always serves a
/// Unix domain socket; kept as a named type (rather than a bare path) for the
/// same reason [`RecordedBackend`] stays an enum: the on-disk schema's `kind`
/// tag is a stable wire contract, not a Rust-only convenience.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Endpoint {
    /// Host-native daemon serving a Unix domain socket. Auth is filesystem
    /// permissions on the socket, so there is no token here.
    Unix { path: PathBuf },
}

/// The backend serving the daemon, mirroring `omnifs_api::DaemonBackend` but
/// owned here so the workspace crate does not depend on the control-API crate.
/// The native variant carries the pid for a liveness-checked sweep.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "backend", rename_all = "lowercase")]
pub enum RecordedBackend {
    Native { pid: u32 },
}

/// A bound TCP namespace attach listener: the Docker Desktop path, where a
/// containerized frontend cannot share this host's Unix socket into the Linux
/// VM it runs in, so it dials TCP loopback with `token` instead. Bound eagerly
/// at daemon start (`--attach-tcp`) or later via `POST /v1/attach-listeners`;
/// absent when TCP attach was never requested, matching the other UDS-only
/// attach transport, which needs no record entry at all (filesystem
/// permissions are its whole auth story).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachRecord {
    pub addr: String,
    pub token: String,
}

/// One serving frontend and where it is mounted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrontendRecord {
    pub kind: FrontendKind,
    pub mount_point: PathBuf,
    /// How this frontend is delivered. Absent for a host-native frontend
    /// (today's only shape); `Some(Via::Docker)` or `Some(Via::Krunkit)` for
    /// the opt-in virtualized FUSE frontend attached to a host-native
    /// daemon's TCP namespace listener.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<Via>,
}

/// How a frontend reaches the shared namespace. Distinct from `RecordedBackend`,
/// which names the daemon's own delivery: a host-native daemon can still host a
/// virtualized-delivered frontend attached over the TCP namespace listener.
/// `Docker` runs a container attached over TCP; `Krunkit` (a libkrun microVM
/// on macOS, see `docs/contracts/40-frontends.md`) runs the same frontend
/// binary in a guest attached over vsock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Via {
    Docker,
    Krunkit,
}

impl Via {
    /// The lowercase name this variant serializes as, for user-facing
    /// messages that need to name the backend without round-tripping through
    /// JSON.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::Krunkit => "krunkit",
        }
    }
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
    /// The bound TCP attach listener, if one was ever requested this start.
    /// Absent (not merely empty) when TCP attach was never bound, so an older
    /// reader sees no field at all rather than a spurious `null`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attach: Option<AttachRecord>,
}

impl RuntimeRecord {
    /// Assemble a record stamped with the current schema version and an
    /// `started_at` of now. `attach` starts absent; a running daemon that later
    /// binds the TCP attach listener patches it in with a read-modify-write
    /// (see the daemon's `ensure_attach_tcp`), preserving this `started_at`.
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
            attach: None,
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

    /// Read the record at `path`, apply `patch`, and write it back atomically,
    /// preserving every field the closure leaves untouched. Returns `Ok(true)`
    /// when a record existed and was rewritten, `Ok(false)` when no record was
    /// present (nothing to patch). The read-modify-write is not serialized
    /// against a concurrent writer: it is for best-effort field patches (the
    /// attach binding, the frontend list) that the daemon rewrites wholesale on
    /// its next restart.
    pub fn update(path: &Path, edit: impl FnOnce(&mut Self)) -> io::Result<bool> {
        let Some(mut record) = Self::read(path)? else {
            return Ok(false);
        };
        edit(&mut record);
        record.write(path)?;
        Ok(true)
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
                via: None,
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
    fn attach_is_absent_by_default_and_not_serialized() {
        let record = sample_native();
        assert_eq!(record.attach, None);
        let json = serde_json::to_value(&record).unwrap();
        assert!(
            json.get("attach").is_none(),
            "an absent attach must not serialize as a null field: {json}"
        );
    }

    #[test]
    fn attach_round_trips_through_disk_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RUNTIME_RECORD_FILE);
        let mut record = sample_native();
        record.attach = Some(AttachRecord {
            addr: "127.0.0.1:54321".to_string(),
            token: "a".repeat(32),
        });
        record.write(&path).unwrap();

        let read = RuntimeRecord::read(&path).unwrap().unwrap();
        assert_eq!(read.attach, record.attach);

        let json = serde_json::to_value(&record).unwrap();
        assert_eq!(json["attach"]["addr"], "127.0.0.1:54321");
        assert_eq!(json["attach"]["token"], "a".repeat(32));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the attach token must stay behind 0600 too");
        }
    }

    #[test]
    fn a_record_written_without_attach_reads_back_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RUNTIME_RECORD_FILE);
        // An older writer's JSON shape: no `attach` key at all.
        std::fs::write(
            &path,
            r#"{"version":1,"endpoint":{"kind":"unix","path":"/x"},"backend":"native","pid":1,"instance_id":"x","frontends":[],"started_at":"2026-07-07T00:00:00Z"}"#,
        )
        .unwrap();
        let read = RuntimeRecord::read(&path).unwrap().unwrap();
        assert_eq!(read.attach, None);
    }

    #[test]
    fn frontend_via_docker_round_trips_and_omits_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RUNTIME_RECORD_FILE);
        let mut record = sample_native();
        record.frontends.push(FrontendRecord {
            kind: FrontendKind::Fuse,
            mount_point: PathBuf::from("/omnifs"),
            via: Some(Via::Docker),
        });
        record.write(&path).unwrap();

        let read = RuntimeRecord::read(&path).unwrap().unwrap();
        assert_eq!(read.frontends[1].via, Some(Via::Docker));
        assert_eq!(read.frontends[0].via, None);

        let json = serde_json::to_value(&record).unwrap();
        assert_eq!(json["frontends"][1]["via"], "docker");
        assert!(
            json["frontends"][0].get("via").is_none(),
            "an absent via must not serialize as a null field: {json}"
        );
    }

    #[test]
    fn frontend_via_krunkit_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RUNTIME_RECORD_FILE);
        let mut record = sample_native();
        record.frontends.push(FrontendRecord {
            kind: FrontendKind::Fuse,
            mount_point: PathBuf::from("/omnifs"),
            via: Some(Via::Krunkit),
        });
        record.write(&path).unwrap();

        let read = RuntimeRecord::read(&path).unwrap().unwrap();
        assert_eq!(read.frontends[1].via, Some(Via::Krunkit));

        let json = serde_json::to_value(&record).unwrap();
        assert_eq!(json["frontends"][1]["via"], "krunkit");
        assert_eq!(Via::Krunkit.label(), "krunkit");
    }

    /// An older writer's JSON shape (no `via` key on a frontend entry) must
    /// still parse, reading back as `None`.
    #[test]
    fn frontend_without_via_reads_back_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RUNTIME_RECORD_FILE);
        std::fs::write(
            &path,
            r#"{"version":1,"endpoint":{"kind":"unix","path":"/x"},"backend":"native","pid":1,"instance_id":"x","frontends":[{"kind":"fuse","mount_point":"/omnifs"}],"started_at":"2026-07-07T00:00:00Z"}"#,
        )
        .unwrap();
        let read = RuntimeRecord::read(&path).unwrap().unwrap();
        assert_eq!(read.frontends[0].via, None);
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

    #[test]
    fn update_patches_in_place_and_preserves_untouched_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RUNTIME_RECORD_FILE);
        let record = sample_native();
        let started_at = record.started_at.clone();
        record.write(&path).unwrap();

        let existed = RuntimeRecord::update(&path, |r| {
            r.attach = Some(AttachRecord {
                addr: "127.0.0.1:7979".to_string(),
                token: "cafef00d".to_string(),
            });
        })
        .unwrap();
        assert!(existed);

        let read = RuntimeRecord::read(&path).unwrap().unwrap();
        assert_eq!(read.attach.as_ref().unwrap().addr, "127.0.0.1:7979");
        // fields the closure did not touch survive verbatim.
        assert_eq!(read.started_at, started_at);
        assert_eq!(read.instance_id, "b1946ac92492d234");
    }

    #[test]
    fn update_on_absent_record_is_a_reported_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RUNTIME_RECORD_FILE);
        let existed =
            RuntimeRecord::update(&path, |_| panic!("must not run on an absent record")).unwrap();
        assert!(!existed);
        assert!(!path.exists());
    }
}
