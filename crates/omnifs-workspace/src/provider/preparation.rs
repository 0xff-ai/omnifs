//! Cross-process coordination and durable progress for provider preparation.
//!
//! The record is operational history for CLI status. It is never proof that a
//! Wasmtime cache entry is valid; loading the exact component remains the
//! authority for that decision.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::PathBuf;

use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::ids::{ProviderId, ProviderName};
use crate::io::{ensure_private_dir, write_atomic};
use crate::provider::IndexEntry;

const RECORD_VERSION: u32 = 1;
const RECORD_FILE: &str = "provider-preparation.json";
const LOCK_FILE: &str = "provider-preparation.lock";
const LOG_FILE: &str = "provider-preparation.log";

/// Workspace-local owner of the preparation lock, record, and log paths.
#[derive(Debug, Clone)]
pub struct Preparation {
    cache_dir: PathBuf,
}

impl Preparation {
    #[must_use]
    pub fn new(cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            cache_dir: cache_dir.into(),
        }
    }

    #[must_use]
    pub fn log_path(&self) -> PathBuf {
        self.cache_dir.join(LOG_FILE)
    }

    #[must_use]
    pub fn record_path(&self) -> PathBuf {
        self.cache_dir.join(RECORD_FILE)
    }

    pub fn read(&self) -> io::Result<Option<Record>> {
        let path = self.record_path();
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let record = serde_json::from_slice::<Record>(&bytes).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "parse provider preparation record {}: {error}",
                    path.display()
                ),
            )
        })?;
        if record.version != RECORD_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "provider preparation record at {} has version {}; this build understands only version {}",
                    path.display(),
                    record.version,
                    RECORD_VERSION
                ),
            ));
        }
        Ok(Some(record))
    }

    pub fn acquire(&self) -> io::Result<Lease> {
        let file = self.open_lock()?;
        file.lock_exclusive()?;
        Ok(Lease {
            file,
            record_path: self.record_path(),
        })
    }

    pub fn try_acquire(&self) -> io::Result<Option<Lease>> {
        let file = self.open_lock()?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(Lease {
                file,
                record_path: self.record_path(),
            })),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Observe whether another process currently owns the preparation lock
    /// without creating workspace state.
    pub fn is_active(&self) -> io::Result<bool> {
        let path = self.cache_dir.join(LOCK_FILE);
        let file = match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        match file.try_lock_exclusive() {
            Ok(()) => {
                fs2::FileExt::unlock(&file)?;
                Ok(false)
            },
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(true),
            Err(error) => Err(error),
        }
    }

    fn open_lock(&self) -> io::Result<File> {
        ensure_private_dir(&self.cache_dir)?;
        let path = self.cache_dir.join(LOCK_FILE);
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        options.open(path)
    }
}

/// Exclusive ownership of one provider preparation run.
pub struct Lease {
    file: File,
    record_path: PathBuf,
}

impl Lease {
    pub fn write(&self, record: &Record) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(record)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        write_atomic(&self.record_path, &bytes, 0o600)
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Running,
    Complete,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderState {
    Preparing,
    Prepared,
    Failed,
}

/// Strict, atomic snapshot of the current or most recent preparation run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub version: u32,
    pub pid: u32,
    pub state: RunState,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    pub providers: Vec<Provider>,
}

impl Record {
    #[must_use]
    pub fn running(entries: impl IntoIterator<Item = IndexEntry>) -> Self {
        let mut providers = entries
            .into_iter()
            .map(|entry| Provider {
                id: entry.id,
                name: entry.name,
                state: ProviderState::Preparing,
                duration_ms: None,
                error: None,
            })
            .collect::<Vec<_>>();
        providers.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then(left.id.as_bytes().cmp(right.id.as_bytes()))
        });
        Self {
            version: RECORD_VERSION,
            pid: std::process::id(),
            state: RunState::Running,
            started_at: now(),
            finished_at: None,
            providers,
        }
    }

    pub fn settle(&mut self, id: ProviderId, duration_ms: u64, error: Option<String>) {
        if let Some(provider) = self.providers.iter_mut().find(|provider| provider.id == id) {
            provider.duration_ms = Some(duration_ms);
            provider.state = if error.is_some() {
                ProviderState::Failed
            } else {
                ProviderState::Prepared
            };
            provider.error = error;
        }
        if self
            .providers
            .iter()
            .all(|provider| provider.state != ProviderState::Preparing)
        {
            self.state = if self
                .providers
                .iter()
                .any(|provider| provider.state == ProviderState::Failed)
            {
                RunState::Failed
            } else {
                RunState::Complete
            };
            self.finished_at = Some(now());
        }
    }

    #[must_use]
    pub fn completed(&self) -> usize {
        self.providers
            .iter()
            .filter(|provider| provider.state != ProviderState::Preparing)
            .count()
    }

    #[must_use]
    pub fn failed(&self) -> usize {
        self.providers
            .iter()
            .filter(|provider| provider.state == ProviderState::Failed)
            .count()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub id: ProviderId,
    pub name: ProviderName,
    pub state: ProviderState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{ProviderName, ProviderVersion};

    fn entry(bytes: &[u8], name: &str) -> IndexEntry {
        IndexEntry {
            id: ProviderId::from_wasm_bytes(bytes),
            name: ProviderName::new(name).unwrap(),
            version: Some(ProviderVersion::new("1")),
        }
    }

    #[test]
    fn lock_is_joinable_across_handles() {
        let dir = tempfile::tempdir().unwrap();
        let first = Preparation::new(dir.path());
        let second = Preparation::new(dir.path());
        let lease = first.try_acquire().unwrap().expect("first lease");
        assert!(second.try_acquire().unwrap().is_none());
        drop(lease);
        assert!(second.try_acquire().unwrap().is_some());
    }

    #[test]
    fn record_round_trips_and_settles_from_provider_results() {
        let dir = tempfile::tempdir().unwrap();
        let preparation = Preparation::new(dir.path());
        let lease = preparation.acquire().unwrap();
        let ok = entry(b"ok", "ok");
        let failed = entry(b"failed", "failed");
        let mut record = Record::running([ok.clone(), failed.clone()]);
        lease.write(&record).unwrap();

        record.settle(ok.id, 10, None);
        record.settle(failed.id, 20, Some("compile failed".to_owned()));
        lease.write(&record).unwrap();

        let loaded = preparation.read().unwrap().unwrap();
        assert_eq!(loaded.state, RunState::Failed);
        assert_eq!(loaded.completed(), 2);
        assert_eq!(loaded.failed(), 1);
    }
}
