use fs2::FileExt as _;
use omnifs_core::{FilesystemSpec, ResourceName};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

const RUNNER_RECORD: &str = "runner.json";
const RUNNER_LOCK: &str = "runner.lock";

#[derive(Debug, thiserror::Error)]
pub enum RunnerRecordError {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported runner record version {0}")]
    UnsupportedVersion(u64),
    #[error("filesystem location is already owned")]
    LocationOwned,
    #[error("runner record already exists at {0}")]
    RecordExists(PathBuf),
    #[error("invalid runner instance id")]
    InvalidInstance,
    #[error("runner pid and process group must be nonzero")]
    InvalidProcess,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerRecord {
    pub version: u8,
    pub instance_id: String,
    pub pid: u32,
    pub process_group: u32,
    pub filesystem: ResourceName,
    pub spec: FilesystemSpec,
    pub control_socket: PathBuf,
}

impl RunnerRecord {
    pub const VERSION: u8 = 2;

    pub fn new(
        instance_id: String,
        filesystem: ResourceName,
        spec: FilesystemSpec,
        control_socket: PathBuf,
    ) -> Result<Self, RunnerRecordError> {
        let pid = std::process::id();
        let process_group = process_group_id()?;
        let record = Self {
            version: Self::VERSION,
            instance_id,
            pid,
            process_group,
            filesystem,
            spec,
            control_socket,
        };
        record.validate()?;
        Ok(record)
    }

    pub fn read(state_dir: &Path) -> Result<Option<Self>, RunnerRecordError> {
        let path = state_dir.join(RUNNER_RECORD);
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let value: serde_json::Value = serde_json::from_reader(file)?;
        if value.get("version").and_then(serde_json::Value::as_u64)
            != Some(u64::from(Self::VERSION))
        {
            return Err(RunnerRecordError::UnsupportedVersion(
                value
                    .get("version")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default(),
            ));
        }
        let record: Self = serde_json::from_value(value)?;
        record.validate()?;
        Ok(Some(record))
    }

    fn validate(&self) -> Result<(), RunnerRecordError> {
        if self.instance_id.len() != 32
            || !self
                .instance_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(RunnerRecordError::InvalidInstance);
        }
        if self.pid == 0 || self.process_group == 0 || self.pid != self.process_group {
            return Err(RunnerRecordError::InvalidProcess);
        }
        Ok(())
    }
}

pub struct RunnerClaim {
    state_dir: PathBuf,
    _file: File,
}

impl RunnerClaim {
    pub fn acquire(state_dir: &Path) -> Result<Self, RunnerRecordError> {
        ensure_private_dir(state_dir)?;
        let path = state_dir.join(RUNNER_LOCK);
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        file.try_lock_exclusive()
            .map_err(|_| RunnerRecordError::LocationOwned)?;
        Ok(Self {
            state_dir: state_dir.to_path_buf(),
            _file: file,
        })
    }

    pub fn publish(self, record: &RunnerRecord) -> Result<RunnerRecordFile, RunnerRecordError> {
        record.validate()?;
        let path = self.state_dir.join(RUNNER_RECORD);
        if path.exists() {
            return Err(RunnerRecordError::RecordExists(path));
        }
        let temporary = self
            .state_dir
            .join(format!(".runner-{}.tmp", record.instance_id));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        if let Err(error) = (|| -> Result<(), RunnerRecordError> {
            serde_json::to_writer_pretty(&mut file, record)?;
            writeln!(file)?;
            file.sync_all()?;
            std::fs::rename(&temporary, &path)?;
            File::open(&self.state_dir)?.sync_all()?;
            Ok(())
        })() {
            let _ = std::fs::remove_file(&temporary);
            return Err(error);
        }
        Ok(RunnerRecordFile {
            path,
            instance_id: record.instance_id.clone(),
            _claim: self,
        })
    }
}

pub struct RunnerRecordFile {
    path: PathBuf,
    instance_id: String,
    _claim: RunnerClaim,
}

impl Drop for RunnerRecordFile {
    fn drop(&mut self) {
        let remove = RunnerRecord::read(
            self.path
                .parent()
                .expect("runner record always has a state directory"),
        )
        .is_ok_and(|record| record.is_some_and(|record| record.instance_id == self.instance_id));
        if remove {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn ensure_private_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Read-only probe for any live member of a recorded process group.
///
/// `EPERM` still proves that the group exists. Only `ESRCH` proves absence.
#[cfg(unix)]
pub fn process_group_exists(process_group: u32) -> io::Result<bool> {
    let process_group = i32::try_from(process_group)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(-process_group), None) {
        Ok(()) | Err(nix::errno::Errno::EPERM) => Ok(true),
        Err(nix::errno::Errno::ESRCH) => Ok(false),
        Err(error) => Err(io::Error::from_raw_os_error(error as i32)),
    }
}

#[cfg(not(unix))]
pub fn process_group_exists(_process_group: u32) -> io::Result<bool> {
    Ok(true)
}

#[cfg(unix)]
fn process_group_id() -> io::Result<u32> {
    u32::try_from(nix::unistd::getpgrp().as_raw())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(not(unix))]
fn process_group_id() -> io::Result<u32> {
    Ok(std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(instance: &str, id: &str, mount: &str) -> RunnerRecord {
        RunnerRecord {
            version: RunnerRecord::VERSION,
            instance_id: instance.to_owned(),
            pid: 42,
            process_group: 42,
            filesystem: ResourceName::new(id).unwrap(),
            spec: FilesystemSpec::new(
                omnifs_core::FilesystemProtocol::Nfs,
                omnifs_core::FilesystemRuntime::Host,
                PathBuf::from(mount),
                None,
                None,
            )
            .unwrap(),
            control_socket: PathBuf::from("/tmp/control.sock"),
        }
    }

    #[test]
    fn location_claim_is_exclusive_and_record_drop_is_instance_guarded() {
        let temp = tempfile::tempdir().unwrap();
        let claim = RunnerClaim::acquire(temp.path()).unwrap();
        assert!(matches!(
            RunnerClaim::acquire(temp.path()),
            Err(RunnerRecordError::LocationOwned)
        ));
        let guard = claim
            .publish(&record(
                "00112233445566778899aabbccddeeff",
                "main",
                "/mnt/a",
            ))
            .unwrap();
        assert_eq!(
            RunnerRecord::read(temp.path())
                .unwrap()
                .unwrap()
                .spec
                .location(),
            Path::new("/mnt/a")
        );
        drop(guard);
        assert!(RunnerRecord::read(temp.path()).unwrap().is_none());
        RunnerClaim::acquire(temp.path()).unwrap();
    }

    #[test]
    fn strict_record_rejects_unknown_fields_and_invalid_instances() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join(RUNNER_RECORD),
            r#"{"version":2,"instance_id":"bad","pid":1,"process_group":1,"filesystem":"main","spec":{"protocol":"nfs","runtime":"host","location":"/mnt","docker_image":null,"libkrun_guest_image":null},"control_socket":"/tmp/c","extra":true}"#,
        )
        .unwrap();
        assert!(RunnerRecord::read(temp.path()).is_err());
        assert!(matches!(
            record("bad", "main", "/mnt").validate(),
            Err(RunnerRecordError::InvalidInstance)
        ));
    }
}
