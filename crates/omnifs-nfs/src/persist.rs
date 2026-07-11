//! Persistence of the NFS filehandle-identity table across a frontend restart.
//!
//! An NFS filehandle is `(generation, id)`. A kernel client holds filehandles
//! across a frontend process restart and expects them to keep decoding; a fresh
//! random `generation` would fail every held handle with `NFS4ERR_FHEXPIRED`
//! (the transport-level ESTALE the frontend must not surface). So the restartable
//! out-of-process runner persists the protocol identity table: the `generation`,
//! the `next_ino` allocation cursor, and one entry per inode id carrying only the
//! protocol-local `{ scope, parent, name, kind }`. No [`NodeId`](omnifs_engine::NodeId)
//! is persisted; ids are meaningless across processes, so a reloaded id
//! re-resolves lazily by walking its parent/name chain through the namespace.
//!
//! # Ownership
//!
//! This schema and its IO live in `omnifs-nfs`, not `omnifs-mtab`. The
//! `omnifs-mtab` state file (`MountState`) is mount *discovery/teardown* state
//! (protocol kind, mount point, address when applicable, and pid) shared by
//! frontend runners and the CLI; this file is NFS
//! *protocol identity* (the filehandle decode table), which the frontend contract
//! keeps in `omnifs-nfs`. It lands in the same NFS state directory next to the
//! mtab mount-state files and follows the same discipline: a `version` field, an
//! unknown version is a hard error, an atomic temp-then-rename write, and 0600
//! mode.
//!
//! # Write policy
//!
//! Correctness first, chatter second. The [`Persister`] debounces table mutations
//! with a short write-behind delay and flushes synchronously on a clean shutdown.
//! Entries allocated within the debounce window immediately before a `SIGKILL`
//! are lost, and their filehandles decode to `NFS4ERR_STALE` after restart. That
//! is acceptable: the durability guarantee covers surviving handles a client
//! actually holds, and a client holds a handle only after an op it observed
//! completing (past the debounce).

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use dashmap::DashMap;
use serde::{Deserialize, Serialize};

use crate::adapter::{ColdEntry, Inode};
use crate::export::NodeKind;
use crate::protocol::consts::{EXPORT_ROOT_ID, ROOT_ID};

/// Basename of the filehandle-table state file inside the NFS state directory. A
/// restart must find its predecessor by a name stable across pids, so this is a
/// fixed name; the restartable runner is given its own state directory per mount.
pub(crate) const FH_STATE_FILE: &str = "filehandles.json";

/// Debounced write-behind delay: a burst of table mutations coalesces into one
/// write this long after the last mutation. Short enough that a client rarely
/// races a `SIGKILL` before its just-allocated handle is durable; long enough
/// that a directory walk does not fsync per entry.
pub(crate) const DEBOUNCE: Duration = Duration::from_millis(100);

/// The persisted filehandle-identity table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct FhState {
    pub version: u8,
    pub generation: u64,
    pub next_ino: u64,
    pub entries: Vec<FhEntry>,
}

impl FhState {
    pub(crate) const VERSION: u8 = 1;

    /// Load a persisted table. `Ok(None)` when the file is absent (a fresh
    /// start). An unreadable or unknown-version file is a hard error rather than a
    /// silent fresh start, so a corrupt or forward-version file is loud.
    pub(crate) fn load(path: &Path) -> Result<Option<Self>, PersistError> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(PersistError::Io(error)),
        };
        let state: Self = serde_json::from_slice(&bytes).map_err(PersistError::Decode)?;
        if state.version != Self::VERSION {
            return Err(PersistError::Version {
                found: state.version,
                expected: Self::VERSION,
            });
        }
        Ok(Some(state))
    }

    /// Atomically encode to a sibling temp file, fsync, and rename over `path`.
    fn write_atomic(&self, path: &Path) -> io::Result<()> {
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        {
            let mut options = std::fs::OpenOptions::new();
            options.create(true).truncate(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&tmp)?;
            let bytes = serde_json::to_vec(self).map_err(io::Error::other)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp, path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
        if let Ok(dir_file) = std::fs::File::open(dir) {
            let _ = dir_file.sync_all();
        }
        Ok(())
    }
}

/// One inode's persistable protocol identity: no [`NodeId`](omnifs_engine::NodeId),
/// only the `{ scope, parent, name, kind }` a reloaded id re-resolves from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct FhEntry {
    pub id: u64,
    pub scope: u64,
    pub parent: u64,
    pub name: String,
    pub kind: NodeKind,
}

impl FhEntry {
    fn from_inode(id: u64, inode: &Inode) -> Self {
        Self {
            id,
            scope: inode.scope,
            parent: inode.parent,
            name: inode.name.clone(),
            kind: inode.kind,
        }
    }

    fn from_cold(id: u64, cold: &ColdEntry) -> Self {
        Self {
            id,
            scope: cold.scope,
            parent: cold.parent,
            name: cold.name.clone(),
            kind: cold.kind,
        }
    }
}

/// Everything the [`Export`](crate::adapter::Export) needs to start the
/// restartable filehandle table: the (reloaded or fresh) generation and
/// allocation cursor, the cold entries to seed, and where to persist. Built by
/// [`mount_blocking`](crate::mount_blocking) only on the out-of-process runner
/// path.
pub(crate) struct PersistInit {
    pub generation: u64,
    pub next_ino: u64,
    pub entries: Vec<FhEntry>,
    pub state_path: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PersistError {
    #[error("filehandle state io error: {0}")]
    Io(#[from] io::Error),
    #[error("filehandle state decode error: {0}")]
    Decode(serde_json::Error),
    #[error("filehandle state version {found} is not the supported version {expected}")]
    Version { found: u8, expected: u8 },
}

/// The live tables the [`Persister`] snapshots. The adapter holds the same `Arc`s,
/// so the persister reads a consistent-enough view without a lock: a snapshot may
/// miss an entry mid-mutation, which the next debounced write picks up.
pub(crate) struct PersistTables {
    pub generation: u64,
    pub next_ino: Arc<AtomicU64>,
    pub inodes: Arc<DashMap<u64, Inode>>,
    pub cold: Arc<DashMap<u64, ColdEntry>>,
}

impl PersistTables {
    /// Build the on-disk table from the current live and cold entries. A live
    /// inode supersedes a same-id cold entry (it carries a resolved body but the
    /// same identity chain). The two export roots are constants re-seeded on load,
    /// so they are omitted.
    fn snapshot(&self) -> FhState {
        let mut by_id: std::collections::HashMap<u64, FhEntry> = std::collections::HashMap::new();
        for entry in self.cold.iter() {
            let id = *entry.key();
            by_id.insert(id, FhEntry::from_cold(id, entry.value()));
        }
        for entry in self.inodes.iter() {
            let id = *entry.key();
            if id == ROOT_ID || id == EXPORT_ROOT_ID {
                continue;
            }
            by_id.insert(id, FhEntry::from_inode(id, entry.value()));
        }
        FhState {
            version: FhState::VERSION,
            generation: self.generation,
            next_ino: self.next_ino.load(Ordering::Relaxed),
            entries: by_id.into_values().collect(),
        }
    }

    fn persist(&self, path: &Path) {
        if let Err(error) = self.snapshot().write_atomic(path) {
            tracing::warn!(%error, path = %path.display(), "failed to persist NFS filehandle table");
        }
    }
}

/// A debounced write-behind persister for the filehandle table. It owns a
/// background thread; marking the table dirty schedules a coalesced write, and
/// dropping the persister flushes synchronously.
pub(crate) struct Persister {
    tx: mpsc::Sender<Cmd>,
    handle: Option<JoinHandle<()>>,
}

enum Cmd {
    Dirty,
    Flush(mpsc::Sender<()>),
}

impl Persister {
    /// Spawn the write-behind thread over `tables`, writing to `path`.
    pub(crate) fn spawn(path: PathBuf, tables: PersistTables) -> Self {
        let (tx, rx) = mpsc::channel::<Cmd>();
        let handle = thread::Builder::new()
            .name("nfs-fh-persist".to_string())
            .spawn(move || run(&path, &tables, &rx))
            .expect("spawn nfs filehandle persister thread");
        Self {
            tx,
            handle: Some(handle),
        }
    }

    /// Signal that the table changed. Coalesced by the debounce window.
    pub(crate) fn mark_dirty(&self) {
        // A closed channel means the persister thread already exited; the next
        // caller path (shutdown) is the only place that matters and it re-flushes.
        let _ = self.tx.send(Cmd::Dirty);
    }
}

impl Drop for Persister {
    fn drop(&mut self) {
        // Flush synchronously so a clean shutdown never loses a debounced write,
        // then let the thread observe the closed channel and exit.
        let (ack_tx, ack_rx) = mpsc::channel();
        if self.tx.send(Cmd::Flush(ack_tx)).is_ok() {
            let _ = ack_rx.recv();
        }
        drop(std::mem::replace(&mut self.tx, mpsc::channel().0));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// The persister thread loop: block for the first mutation, then debounce a burst
/// into one write; a flush writes immediately and acks.
fn run(path: &Path, tables: &PersistTables, rx: &mpsc::Receiver<Cmd>) {
    let mut dirty = false;
    loop {
        let cmd = if dirty {
            match rx.recv_timeout(DEBOUNCE) {
                Ok(cmd) => cmd,
                Err(RecvTimeoutError::Timeout) => {
                    tables.persist(path);
                    dirty = false;
                    continue;
                },
                Err(RecvTimeoutError::Disconnected) => {
                    tables.persist(path);
                    return;
                },
            }
        } else {
            match rx.recv() {
                Ok(cmd) => cmd,
                Err(_) => return,
            }
        };
        match cmd {
            Cmd::Dirty => dirty = true,
            Cmd::Flush(ack) => {
                tables.persist(path);
                dirty = false;
                let _ = ack.send(());
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "omnifs-nfs-fh-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn sample() -> FhState {
        FhState {
            version: FhState::VERSION,
            generation: 0xDEAD_BEEF,
            next_ino: 42,
            entries: vec![
                FhEntry {
                    id: 10,
                    scope: ROOT_ID,
                    parent: ROOT_ID,
                    name: "test".to_string(),
                    kind: NodeKind::Directory,
                },
                FhEntry {
                    id: 11,
                    scope: ROOT_ID,
                    parent: 10,
                    name: "message".to_string(),
                    kind: NodeKind::File,
                },
            ],
        }
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = temp_dir();
        let path = dir.join(FH_STATE_FILE);
        let state = sample();
        state.write_atomic(&path).expect("write");
        let loaded = FhState::load(&path).expect("load").expect("present");
        assert_eq!(loaded, state);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_is_fresh_start() {
        let dir = temp_dir();
        let path = dir.join(FH_STATE_FILE);
        assert!(FhState::load(&path).expect("load").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unknown_version_is_error() {
        let dir = temp_dir();
        let path = dir.join(FH_STATE_FILE);
        let mut state = sample();
        state.version = 99;
        std::fs::write(&path, serde_json::to_vec(&state).expect("encode")).expect("write");
        assert!(matches!(
            FhState::load(&path),
            Err(PersistError::Version {
                found: 99,
                expected: 1
            })
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn state_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir();
        let path = dir.join(FH_STATE_FILE);
        sample().write_atomic(&path).expect("write");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::remove_dir_all(&dir).ok();
    }
}
