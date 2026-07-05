//! Persisted launch record written at `<config_dir>/launch.json`.
//!
//! `up` and `dev` write it once the daemon is ready; a clean `down` removes
//! it. When the daemon is dead and `down` cannot probe the control port, it
//! reads this record to know what to sweep, instead of recomputing defaults
//! from `[system].runtime`.
//!
//! An unknown `version` field is reported and treated as an error rather than
//! silently ignored, matching the NFS mount-state version discipline.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use omnifs_api::{DaemonBackend, DaemonStatus};
use serde::{Deserialize, Serialize};

use crate::launch_backend::{DockerTarget, LaunchBackend};

/// Schema version this CLI understands. A bump here means the CLI writing the
/// record knows something the current CLI does not; the current CLI reports
/// that instead of silently ignoring a field.
const RECORD_VERSION: u32 = 1;

/// The persisted form of launch parameters. Stored as JSON at
/// `<config_dir>/launch.json`.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct LaunchRecord {
    version: u32,
    #[serde(flatten)]
    backend: RecordedBackend,
    control_addr: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    mount_point: Option<PathBuf>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "backend", rename_all = "lowercase")]
enum RecordedBackend {
    Native {
        daemon_pid: u32,
    },
    Docker {
        container_name: String,
        image: String,
    },
}

impl LaunchRecord {
    /// Cache the backend identity reported by a ready daemon.
    pub(crate) fn from_status(
        status: &DaemonStatus,
        control_addr: std::net::SocketAddr,
    ) -> Result<Self> {
        let backend = match &status.backend {
            DaemonBackend::Native { pid } => RecordedBackend::Native { daemon_pid: *pid },
            DaemonBackend::Docker {
                container_name,
                image,
            } => {
                DockerTarget::new(container_name.clone(), image.clone())
                    .context("daemon reported invalid Docker backend identity")?;
                RecordedBackend::Docker {
                    container_name: container_name.clone(),
                    image: image.clone(),
                }
            },
        };
        Ok(Self {
            version: RECORD_VERSION,
            backend,
            control_addr: control_addr.to_string(),
            mount_point: Some(status.mount_point.clone()),
        })
    }

    /// Atomically write to `<config_dir>/launch.json` through the workspace's
    /// one atomic writer (temp + rename).
    pub(crate) fn write(&self, config_dir: &Path) -> Result<()> {
        let target = record_path(config_dir);
        let json = serde_json::to_string_pretty(self).context("serialize launch record")?;
        std::fs::create_dir_all(config_dir)
            .with_context(|| format!("create config dir {}", config_dir.display()))?;
        omnifs_workspace::io::write_atomic(&target, json.as_bytes(), 0o644)
            .with_context(|| format!("write launch record to {}", target.display()))?;
        Ok(())
    }

    /// Read the record at `<config_dir>/launch.json`. Returns `None` if the
    /// file does not exist (no daemon was started by this CLI). Returns an
    /// error when the file is present but unreadable, unparseable, or carries
    /// a version this CLI does not understand.
    pub(crate) fn read(config_dir: &Path) -> Result<Option<Self>> {
        let path = record_path(config_dir);
        let bytes = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(e).with_context(|| format!("read launch record {}", path.display()));
            },
        };
        let record: Self = serde_json::from_str(&bytes)
            .with_context(|| format!("parse launch record {}", path.display()))?;
        if record.version != RECORD_VERSION {
            anyhow::bail!(
                "launch record at {} has version {}; this CLI understands only version {}. \
                 Run `omnifs down` with the CLI that started the daemon, or delete {} manually \
                 and then run `omnifs down`.",
                path.display(),
                record.version,
                RECORD_VERSION,
                path.display(),
            );
        }
        Ok(Some(record))
    }

    /// Remove the record at `<config_dir>/launch.json`. Idempotent: a missing
    /// file is not an error.
    pub(crate) fn remove(config_dir: &Path) -> Result<()> {
        let path = record_path(config_dir);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("remove launch record {}", path.display())),
        }
    }

    /// Mount point recorded at launch time.
    pub(crate) fn mount_point(&self) -> Option<&Path> {
        self.mount_point.as_deref()
    }

    /// The container name when the daemon runs under the Docker backend, whose
    /// mount lives inside the container and is not host-visible. `None` for the
    /// host-native backend, whose mount point is reachable directly.
    pub(crate) fn container_name(&self) -> Option<&str> {
        match &self.backend {
            RecordedBackend::Docker { container_name, .. } => Some(container_name),
            RecordedBackend::Native { .. } => None,
        }
    }

    /// Human label for the backend the daemon was launched with, read straight
    /// from the run-state file so callers learn the mode without probing.
    pub(crate) fn mode_label(&self) -> &'static str {
        match self.backend {
            RecordedBackend::Native { .. } => "native",
            RecordedBackend::Docker { .. } => "container",
        }
    }

    /// Reconstruct the backend variant from the record so `down`/`reset` can
    /// dispatch through `LaunchBackend::reclaim` without naming native or Docker.
    pub(crate) fn into_backend(self) -> Result<LaunchBackend> {
        match self.backend {
            RecordedBackend::Native { .. } => Ok(LaunchBackend::Native),
            RecordedBackend::Docker {
                container_name,
                image,
            } => Ok(LaunchBackend::Docker(DockerTarget::new(
                container_name,
                image,
            )?)),
        }
    }
}

fn record_path(config_dir: &Path) -> PathBuf {
    config_dir.join("launch.json")
}
