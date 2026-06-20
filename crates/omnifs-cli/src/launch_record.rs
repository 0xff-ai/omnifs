//! Persisted launch record written at `<config_dir>/launch.json`.
//!
//! `up` and `dev` write it once the daemon is ready; a clean `down` removes
//! it. When the daemon is dead and `down` cannot probe the control port, it
//! reads this record to know what to sweep, instead of recomputing defaults
//! from `[system].runtime`.
//!
//! An unknown `version` field is reported and treated as an error rather than
//! silently ignored, matching the NFS `STATE_VERSION` discipline.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use omnifs_api::LaunchKind;
use serde::{Deserialize, Serialize};

use crate::backend::{Backend, LaunchParams};
use crate::container_name::ContainerName;
use crate::image_ref::ImageRef;

/// Schema version this CLI understands. A bump here means the CLI writing the
/// record knows something the current CLI does not; the current CLI reports
/// that instead of silently ignoring a field.
const RECORD_VERSION: u32 = 1;

/// The persisted form of launch parameters. Stored as JSON at
/// `<config_dir>/launch.json`.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct LaunchRecord {
    version: u32,
    runtime: RuntimeKind,
    control_addr: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    mount_point: Option<PathBuf>,
    /// Native only; `null` for Docker.
    #[serde(skip_serializing_if = "Option::is_none")]
    daemon_pid: Option<u32>,
    /// Docker only; `null` for native.
    #[serde(skip_serializing_if = "Option::is_none")]
    container_name: Option<String>,
    /// Docker only; `null` for native.
    #[serde(skip_serializing_if = "Option::is_none")]
    image: Option<String>,
    started_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum RuntimeKind {
    Native,
    Docker,
}

impl LaunchRecord {
    /// Build a record from launch params. `daemon_pid` is the PID of the
    /// spawned daemon on the native path; pass `None` for Docker.
    pub(crate) fn new(params: &LaunchParams, daemon_pid: Option<u32>) -> Result<Self> {
        let runtime = match &params.backend {
            Backend::Native => RuntimeKind::Native,
            Backend::Docker { .. } => RuntimeKind::Docker,
        };
        let container_name = match &params.backend {
            Backend::Docker { container_name, .. } => Some(container_name.as_str().to_string()),
            Backend::Native => None,
        };
        let image = match &params.backend {
            Backend::Docker { image, .. } => Some(image.as_str().to_string()),
            Backend::Native => None,
        };
        let started_at = now_rfc3339()?;
        Ok(Self {
            version: RECORD_VERSION,
            runtime,
            control_addr: params.control_addr.to_string(),
            mount_point: params.mount_point.clone(),
            daemon_pid,
            container_name,
            image,
            started_at,
        })
    }

    /// Atomically write to `<config_dir>/launch.json` (temp + rename).
    pub(crate) fn write(&self, config_dir: &Path) -> Result<()> {
        let target = record_path(config_dir);
        let tmp = target.with_extension("json.tmp");

        let json = serde_json::to_string_pretty(self).context("serialize launch record")?;
        std::fs::create_dir_all(config_dir)
            .with_context(|| format!("create config dir {}", config_dir.display()))?;
        std::fs::write(&tmp, &json)
            .with_context(|| format!("write launch record to {}", tmp.display()))?;
        std::fs::rename(&tmp, &target)
            .with_context(|| format!("rename {} -> {}", tmp.display(), target.display()))?;
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

    /// Human label for the backend the daemon was launched with, read straight
    /// from the run-state file so callers learn the mode without probing.
    pub(crate) fn mode_label(&self) -> &'static str {
        match self.runtime {
            RuntimeKind::Native => "native",
            RuntimeKind::Docker => "container",
        }
    }

    /// Reconstruct the `Backend` variant from the record so `down`/`reset` can
    /// dispatch through `Backend::reclaim` without naming native or Docker.
    pub(crate) fn into_backend(self) -> Result<Backend> {
        match self.runtime {
            RuntimeKind::Native => Ok(Backend::Native),
            RuntimeKind::Docker => {
                let name = self.container_name.ok_or_else(|| {
                    anyhow::anyhow!("launch record is Docker but missing container_name")
                })?;
                let image = self
                    .image
                    .ok_or_else(|| anyhow::anyhow!("launch record is Docker but missing image"))?;
                Ok(Backend::Docker {
                    container_name: ContainerName::new(name)?,
                    image: ImageRef::new(image)?,
                })
            },
        }
    }
}

/// Reconstruct the `Backend` from a daemon's `LaunchKind` plus the launch
/// record, so `down`/`reset` dispatch teardown without naming native or Docker.
///
/// A host-native daemon needs no record. A container daemon reads the record for
/// its container name and image; an absent, unreadable, or incomplete record
/// falls back to the default container name (with a warning), so a corrupt
/// record never strands a running container after the daemon has already been
/// asked to shut down.
pub(crate) fn backend_from_launch_kind(launch: LaunchKind, config_dir: &Path) -> Result<Backend> {
    match launch {
        LaunchKind::HostNative => Ok(Backend::Native),
        LaunchKind::Container => {
            match LaunchRecord::read(config_dir)
                .and_then(|record| record.map(LaunchRecord::into_backend).transpose())
            {
                Ok(Some(backend)) => Ok(backend),
                Ok(None) => default_docker_backend(),
                Err(error) => {
                    anstream::eprintln!(
                        "warning: launch record unreadable ({error:#}); \
                         using default container name `{}`",
                        crate::session::CONTAINER_NAME
                    );
                    default_docker_backend()
                },
            }
        },
    }
}

fn default_docker_backend() -> Result<Backend> {
    Ok(Backend::Docker {
        container_name: ContainerName::new(crate::session::CONTAINER_NAME)?,
        image: ImageRef::new(crate::session::IMAGE)?,
    })
}

fn record_path(config_dir: &Path) -> PathBuf {
    config_dir.join("launch.json")
}

/// Current UTC time as an RFC 3339 string.
fn now_rfc3339() -> Result<String> {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .context("format current time as RFC 3339")
}
