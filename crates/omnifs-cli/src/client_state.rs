//! CLI-private identity and the single-record mutation journal.

use anyhow::Context as _;
use atomic_write_file::OpenOptions as AtomicOpenOptions;
use atomic_write_file::unix::OpenOptionsExt as _;
use fs2::FileExt as _;
use omnifs_bootstrap::{Bootstrap, Client};
use omnifs_core::{ClientOwnerId, MutationId};
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::PathBuf;

const CLIENT_DIR: &str = "client";
const OWNER_FILE: &str = "owner-id";
const OWNER_LOCK: &str = "owner-id.lock";
const MUTATION_FILE: &str = "mutations.json";
const MUTATION_LOCK: &str = "mutations.lock";

/// One op inside the journaled batch, named the same way the daemon and the
/// runner do: a stable `kind` string plus the human target it acted on.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PendingOp {
    pub(crate) kind: String,
    pub(crate) target: String,
}

/// The one mutation batch this client may have in flight. Its presence on
/// disk means the client submitted `ApplyMutation` for this id and does not
/// yet know whether it committed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PendingMutation {
    pub(crate) id: MutationId,
    pub(crate) ops: Vec<PendingOp>,
}

pub(crate) struct ClientState {
    root: PathBuf,
}

impl ClientState {
    pub(crate) fn resolve() -> anyhow::Result<Self> {
        let endpoint = Bootstrap::<Client>::for_client()?;
        Ok(Self {
            root: endpoint.bootstrap_dir().join(CLIENT_DIR),
        })
    }

    pub(crate) fn owner_id(&self) -> anyhow::Result<ClientOwnerId> {
        self.prepare()?;
        let lock_path = self.root.join(OWNER_LOCK);
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
            .with_context(|| format!("open client identity lock {}", lock_path.display()))?;
        std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restrict client identity lock {}", lock_path.display()))?;
        lock.lock_exclusive()
            .with_context(|| format!("lock client identity {}", lock_path.display()))?;

        let path = self.root.join(OWNER_FILE);
        match std::fs::read_to_string(&path) {
            Ok(value) => value
                .trim()
                .parse()
                .with_context(|| format!("parse client owner identity {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut bytes = [0_u8; 16];
                getrandom::fill(&mut bytes).context("generate client owner identity")?;
                let owner = ClientOwnerId::from_bytes(bytes);
                let mut options = AtomicOpenOptions::new();
                options.preserve_mode(false).mode(0o600);
                let mut file = options
                    .open(&path)
                    .with_context(|| format!("create client owner identity {}", path.display()))?;
                writeln!(file, "{owner}").context("write client owner identity")?;
                file.commit().context("commit client owner identity")?;
                Ok(owner)
            },
            Err(error) => {
                Err(error).with_context(|| format!("read client owner identity {}", path.display()))
            },
        }
    }

    /// The single pending mutation record, if a prior command left one.
    pub(crate) fn pending(&self) -> anyhow::Result<Option<PendingMutation>> {
        let _lock = self.lock_mutations()?;
        read_pending(&self.mutations_path())
    }

    /// Replace the pending record: a fresh command is about to apply a
    /// batch under a lease it just acquired.
    pub(crate) fn set_pending(&self, pending: &PendingMutation) -> anyhow::Result<()> {
        let _lock = self.lock_mutations()?;
        write_pending(&self.mutations_path(), pending)
    }

    /// Clear the pending record: the batch settled, one way or the other.
    pub(crate) fn clear_pending(&self) -> anyhow::Result<()> {
        let _lock = self.lock_mutations()?;
        let path = self.mutations_path();
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => {
                Err(error).with_context(|| format!("clear mutation state {}", path.display()))
            },
        }
    }

    fn mutations_path(&self) -> PathBuf {
        self.root.join(MUTATION_FILE)
    }

    fn lock_mutations(&self) -> anyhow::Result<std::fs::File> {
        self.prepare()?;
        let lock_path = self.root.join(MUTATION_LOCK);
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
            .with_context(|| format!("open mutation lock {}", lock_path.display()))?;
        std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restrict mutation lock {}", lock_path.display()))?;
        lock.lock_exclusive()
            .with_context(|| format!("lock mutation state {}", lock_path.display()))?;
        Ok(lock)
    }

    fn prepare(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("create client state {}", self.root.display()))?;
        std::fs::set_permissions(&self.root, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restrict client state {}", self.root.display()))
    }
}

fn read_pending(path: &std::path::Path) -> anyhow::Result<Option<PendingMutation>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .with_context(|| format!("parse mutation state {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read mutation state {}", path.display())),
    }
}

fn write_pending(path: &std::path::Path, pending: &PendingMutation) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(pending).context("encode mutation state")?;
    let mut options = AtomicOpenOptions::new();
    options.preserve_mode(false).mode(0o600);
    let mut file = options
        .open(path)
        .with_context(|| format!("open mutation state {}", path.display()))?;
    file.write_all(&bytes)
        .with_context(|| format!("write mutation state {}", path.display()))?;
    file.commit()
        .with_context(|| format!("commit mutation state {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_identity_is_stable_and_private() {
        let temp = tempfile::tempdir().unwrap();
        let state = ClientState {
            root: temp.path().join("client"),
        };
        let first = state.owner_id().unwrap();
        assert_eq!(state.owner_id().unwrap(), first);
        assert_eq!(
            std::fs::metadata(state.root.join(OWNER_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&state.root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(state.root.join(OWNER_LOCK))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn no_journal_file_means_no_pending_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let state = ClientState {
            root: temp.path().join("client"),
        };
        assert!(state.pending().unwrap().is_none());
        assert!(!state.root.join(MUTATION_FILE).exists());
    }

    #[test]
    fn pending_round_trips_and_clears() {
        let temp = tempfile::tempdir().unwrap();
        let state = ClientState {
            root: temp.path().join("client"),
        };
        let record = PendingMutation {
            id: MutationId::from_bytes([7; 16]),
            ops: vec![PendingOp {
                kind: "mount.create".to_owned(),
                target: "github".to_owned(),
            }],
        };
        state.set_pending(&record).unwrap();
        let read_back = state.pending().unwrap().expect("pending record");
        assert_eq!(read_back.id, record.id);
        assert_eq!(read_back.ops.len(), 1);
        assert_eq!(read_back.ops[0].kind, "mount.create");
        assert_eq!(read_back.ops[0].target, "github");
        assert_eq!(
            std::fs::metadata(state.root.join(MUTATION_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        state.clear_pending().unwrap();
        assert!(state.pending().unwrap().is_none());
        assert!(!state.root.join(MUTATION_FILE).exists());
    }

    #[test]
    fn clearing_an_already_absent_journal_is_not_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let state = ClientState {
            root: temp.path().join("client"),
        };
        state.clear_pending().unwrap();
        state.clear_pending().unwrap();
    }

    #[test]
    fn setting_pending_replaces_any_prior_record() {
        let temp = tempfile::tempdir().unwrap();
        let state = ClientState {
            root: temp.path().join("client"),
        };
        state
            .set_pending(&PendingMutation {
                id: MutationId::from_bytes([1; 16]),
                ops: vec![PendingOp {
                    kind: "mount.remove".to_owned(),
                    target: "old".to_owned(),
                }],
            })
            .unwrap();
        state
            .set_pending(&PendingMutation {
                id: MutationId::from_bytes([2; 16]),
                ops: vec![PendingOp {
                    kind: "credential.submit".to_owned(),
                    target: "github:oauth:default".to_owned(),
                }],
            })
            .unwrap();
        let read_back = state.pending().unwrap().expect("pending record");
        assert_eq!(read_back.id, MutationId::from_bytes([2; 16]));
        assert_eq!(read_back.ops[0].kind, "credential.submit");
    }
}
