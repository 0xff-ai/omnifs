use crate::protocol::consts::{
    ACCESS4_EXECUTE, ACCESS4_LOOKUP, ACCESS4_READ, NF4DIR, NF4LNK, NF4REG, NFS4ERR_ACCESS,
    NFS4ERR_BAD_COOKIE, NFS4ERR_BAD_STATEID, NFS4ERR_BADHANDLE, NFS4ERR_DELAY, NFS4ERR_EXPIRED,
    NFS4ERR_FHEXPIRED, NFS4ERR_INVAL, NFS4ERR_IO, NFS4ERR_ISDIR, NFS4ERR_LOCK_NOTSUPP,
    NFS4ERR_MINOR_VERS_MISMATCH, NFS4ERR_NO_GRACE, NFS4ERR_NOENT, NFS4ERR_NOFILEHANDLE,
    NFS4ERR_NOT_SAME, NFS4ERR_NOTDIR, NFS4ERR_NOTSUPP, NFS4ERR_OLD_STATEID, NFS4ERR_OP_ILLEGAL,
    NFS4ERR_OPENMODE, NFS4ERR_RESOURCE, NFS4ERR_ROFS, NFS4ERR_STALE, NFS4ERR_STALE_CLIENTID,
    NFS4ERR_SYMLINK, NFS4ERR_TOOSMALL, OPEN_STATE_LEASE_SECONDS, OPEN4_SHARE_ACCESS_READ,
};
use dashmap::DashMap;
use omnifs_engine::namespace::{NsError, NsRetryClass};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

pub type StatusResult<T> = Result<T, Status>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Access,
    BadCookie,
    BadHandle,
    BadStateId,
    Delay,
    Expired,
    FhExpired,
    Invalid,
    Io,
    IsDir,
    LockNotSupported,
    MinorVersionMismatch,
    NoEnt,
    NoFileHandle,
    NoGrace,
    NotDir,
    NotSame,
    NotSupported,
    OldStateId,
    OpenMode,
    OpIllegal,
    ReadOnlyFs,
    Resource,
    Stale,
    StaleClientId,
    Symlink,
    TooSmall,
}

impl Status {
    pub(crate) fn wire(self) -> u32 {
        match self {
            Self::Access => NFS4ERR_ACCESS,
            Self::BadCookie => NFS4ERR_BAD_COOKIE,
            Self::BadHandle => NFS4ERR_BADHANDLE,
            Self::BadStateId => NFS4ERR_BAD_STATEID,
            Self::Delay => NFS4ERR_DELAY,
            Self::Expired => NFS4ERR_EXPIRED,
            Self::FhExpired => NFS4ERR_FHEXPIRED,
            Self::Invalid => NFS4ERR_INVAL,
            Self::Io => NFS4ERR_IO,
            Self::IsDir => NFS4ERR_ISDIR,
            Self::LockNotSupported => NFS4ERR_LOCK_NOTSUPP,
            Self::MinorVersionMismatch => NFS4ERR_MINOR_VERS_MISMATCH,
            Self::NoEnt => NFS4ERR_NOENT,
            Self::NoFileHandle => NFS4ERR_NOFILEHANDLE,
            Self::NoGrace => NFS4ERR_NO_GRACE,
            Self::NotDir => NFS4ERR_NOTDIR,
            Self::NotSame => NFS4ERR_NOT_SAME,
            Self::NotSupported => NFS4ERR_NOTSUPP,
            Self::OldStateId => NFS4ERR_OLD_STATEID,
            Self::OpenMode => NFS4ERR_OPENMODE,
            Self::OpIllegal => NFS4ERR_OP_ILLEGAL,
            Self::ReadOnlyFs => NFS4ERR_ROFS,
            Self::Resource => NFS4ERR_RESOURCE,
            Self::Stale => NFS4ERR_STALE,
            Self::StaleClientId => NFS4ERR_STALE_CLIENTID,
            Self::Symlink => NFS4ERR_SYMLINK,
            Self::TooSmall => NFS4ERR_TOOSMALL,
        }
    }
}

/// Reactive NFS status mapping for definitive and transient [`NsError`] kinds.
///
/// `RateLimited`, `Timeout`, and `Network` map to [`Status::Delay`] so the client
/// retries. This path does not spawn background work; proactive `READDIR` deferral
/// is separate (`omnifs_engine::singleflight::Deferred`).
impl From<&NsError> for Status {
    fn from(error: &NsError) -> Self {
        match error.retry_class() {
            NsRetryClass::Retry => Self::Delay,
            NsRetryClass::TooLarge => Self::Resource,
            NsRetryClass::Gone => match error {
                NsError::NotFound => Self::NoEnt,
                NsError::NotDirectory => Self::NotDir,
                NsError::IsDirectory => Self::IsDir,
                _ => Self::Io,
            },
            NsRetryClass::Terminal => match error {
                NsError::Permission => Self::Access,
                NsError::Invalid => Self::Invalid,
                _ => Self::Io,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum NodeKind {
    Directory,
    File,
    Symlink,
}

impl NodeKind {
    pub(crate) fn nfs_type(self) -> u32 {
        match self {
            Self::Directory => NF4DIR,
            Self::File => NF4REG,
            Self::Symlink => NF4LNK,
        }
    }

    pub(crate) fn allowed_access(self) -> u32 {
        match self {
            Self::Directory => ACCESS4_READ | ACCESS4_LOOKUP | ACCESS4_EXECUTE,
            Self::File | Self::Symlink => ACCESS4_READ,
        }
    }

    pub(crate) fn mode(self) -> u32 {
        match self {
            Self::Directory => 0o555,
            Self::File => 0o444,
            Self::Symlink => 0o777,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Attr {
    pub id: u64,
    pub parent: u64,
    pub kind: NodeKind,
    pub size: u64,
    pub mode: u32,
    pub change: u64,
    pub mtime_sec: i64,
}

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub id: u64,
    pub name: String,
    pub attr: Attr,
}

/// A snapshot of a directory's children as observed by the export.
///
/// `exhaustive` reflects whether the underlying source enumerated every entry
/// the directory currently contains. The NFS protocol layer still returns the
/// finite known snapshot as a normal directory listing, because NFS has no
/// useful way to advertise lookup-only dynamic children to shell tools.
#[derive(Debug, Clone)]
pub struct DirListing {
    pub entries: Vec<DirEntry>,
    pub exhaustive: bool,
}

#[derive(Debug, Clone)]
pub struct OpenResult {
    pub stateid: StateId,
    pub attr: Attr,
}

#[derive(Debug, Clone)]
pub struct OpenRead {
    pub id: u64,
    pub data: Vec<u8>,
    pub eof: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateId {
    seqid: u32,
    other: StateIdOther,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StateIdOther([u8; 12]);

impl StateId {
    pub(crate) fn new(seqid: u32, generation: u64, open_id: u32) -> Self {
        let mut other = [0_u8; 12];
        other[..8].copy_from_slice(&generation.to_be_bytes());
        other[8..].copy_from_slice(&open_id.to_be_bytes());
        Self {
            seqid,
            other: StateIdOther(other),
        }
    }

    pub(crate) fn from_wire(raw: &[u8]) -> StatusResult<Self> {
        if raw.len() != 16 {
            return Err(Status::BadStateId);
        }
        let mut seqid = [0_u8; 4];
        seqid.copy_from_slice(&raw[..4]);
        let mut other = [0_u8; 12];
        other.copy_from_slice(&raw[4..]);
        Ok(Self {
            seqid: u32::from_be_bytes(seqid),
            other: StateIdOther(other),
        })
    }

    pub(crate) fn to_wire(self) -> [u8; 16] {
        let mut raw = [0_u8; 16];
        raw[..4].copy_from_slice(&self.seqid.to_be_bytes());
        raw[4..].copy_from_slice(&self.other.0);
        raw
    }

    fn next(self) -> Self {
        Self {
            seqid: self.seqid.saturating_add(1),
            other: self.other,
        }
    }
}

pub(crate) struct OpenTable<B> {
    next: AtomicU32,
    states: DashMap<StateIdOther, OpenState<B>>,
}

pub(crate) struct OpenState<B> {
    pub(crate) inode: u64,
    pub(crate) clientid: u64,
    pub(crate) access: u32,
    pub(crate) body: B,
    seqid: u32,
    renewed_at: Instant,
}

pub(crate) struct OpenSeed<B> {
    pub(crate) generation: u64,
    pub(crate) inode: u64,
    pub(crate) clientid: u64,
    pub(crate) access: u32,
    pub(crate) body: B,
}

impl<B> OpenTable<B> {
    pub(crate) fn new() -> Self {
        Self {
            next: AtomicU32::new(1),
            states: DashMap::new(),
        }
    }

    pub(crate) fn open(&self, seed: OpenSeed<B>) -> StateId {
        let open_id = self.next.fetch_add(1, Ordering::Relaxed);
        let stateid = StateId::new(1, seed.generation, open_id);
        self.states.insert(
            stateid.other,
            OpenState {
                inode: seed.inode,
                clientid: seed.clientid,
                access: seed.access,
                body: seed.body,
                seqid: 1,
                renewed_at: Instant::now(),
            },
        );
        stateid
    }

    pub(crate) fn touch(&self, stateid: StateId) -> StatusResult<()> {
        self.with_state(stateid, |_| ())
    }

    pub(crate) fn with_state<T>(
        &self,
        stateid: StateId,
        f: impl FnOnce(&mut OpenState<B>) -> T,
    ) -> StatusResult<T> {
        let Some(mut state) = self.states.get_mut(&stateid.other) else {
            return Err(Status::BadStateId);
        };
        if state.seqid != stateid.seqid {
            return Err(Status::OldStateId);
        }
        if state.renewed_at.elapsed() > lease_duration() {
            return Err(Status::Expired);
        }
        state.renewed_at = Instant::now();
        Ok(f(&mut state))
    }

    pub(crate) fn close(&self, stateid: StateId) -> StatusResult<(StateId, B)> {
        use dashmap::mapref::entry::Entry;

        // Holding the entry write lock from the seqid/lease check through
        // remove() keeps the check-then-remove race-free without needing a
        // separate get/remove sequence.
        match self.states.entry(stateid.other) {
            Entry::Occupied(occupied) => {
                let state = occupied.get();
                if state.seqid != stateid.seqid {
                    return Err(Status::OldStateId);
                }
                if state.renewed_at.elapsed() > lease_duration() {
                    return Err(Status::Expired);
                }
                let (_, state) = occupied.remove_entry();
                Ok((stateid.next(), state.body))
            },
            Entry::Vacant(_) => Err(Status::BadStateId),
        }
    }

    pub(crate) fn remove_body(&self, stateid: StateId) -> Option<B> {
        self.states
            .remove(&stateid.other)
            .map(|(_, state)| state.body)
    }

    pub(crate) fn renew_client(&self, clientid: u64) {
        for mut state in self.states.iter_mut() {
            if state.clientid == clientid {
                state.renewed_at = Instant::now();
            }
        }
    }

    pub(crate) fn remove_inodes(&self, inodes: &[u64]) -> Vec<B> {
        self.remove_where(|state| inodes.contains(&state.inode))
    }

    pub(crate) fn remove_where(
        &self,
        mut should_remove: impl FnMut(&OpenState<B>) -> bool,
    ) -> Vec<B> {
        let stale = self
            .states
            .iter()
            .filter_map(|entry| should_remove(entry.value()).then(|| *entry.key()))
            .collect::<Vec<_>>();
        stale
            .into_iter()
            .filter_map(|key| self.states.remove(&key).map(|(_, state)| state.body))
            .collect()
    }
}

#[cfg(test)]
impl<B: AsRef<[u8]>> OpenTable<B> {
    pub(crate) fn read(&self, stateid: StateId, offset: u64, count: u32) -> StatusResult<OpenRead> {
        self.with_state(stateid, |state| {
            ensure_read_access(state.access)?;
            let (data, eof) = open_data_slice(state.body.as_ref(), offset, count);
            Ok(OpenRead {
                id: state.inode,
                data,
                eof,
            })
        })?
    }
}

#[cfg(test)]
pub(crate) fn open_data_slice(data: &[u8], offset: u64, count: u32) -> (Vec<u8>, bool) {
    let start = usize::try_from(offset).unwrap_or(usize::MAX);
    if start >= data.len() {
        return (Vec::new(), true);
    }
    let count = usize::try_from(count.min(crate::protocol::consts::MAX_NFS_READ_BYTES))
        .unwrap_or(usize::MAX);
    let end = start.saturating_add(count).min(data.len());
    (data[start..end].to_vec(), end >= data.len())
}

pub(crate) fn ensure_read_access(access: u32) -> StatusResult<()> {
    if access & OPEN4_SHARE_ACCESS_READ == 0 {
        return Err(Status::OpenMode);
    }
    Ok(())
}

fn lease_duration() -> Duration {
    Duration::from_secs(OPEN_STATE_LEASE_SECONDS)
}

pub trait ReadOnlyExport: Send + Sync {
    fn generation(&self) -> u64;
    fn set_clientid(&self, verifier: [u8; 8], owner: Vec<u8>) -> (u64, [u8; 8]);
    fn confirm_client(&self, clientid: u64, verifier: &[u8]) -> StatusResult<()>;
    fn client_confirmed(&self, clientid: u64) -> bool;
    fn root(&self) -> u64;
    fn attr(&self, id: u64) -> StatusResult<Attr>;
    fn lookup(&self, parent: u64, name: &str) -> StatusResult<u64>;
    fn readdir(&self, id: u64) -> StatusResult<DirListing>;
    fn read(&self, id: u64) -> StatusResult<Vec<u8>>;
    fn readlink(&self, id: u64) -> StatusResult<Vec<u8>>;
    fn open_state(&self, id: u64, clientid: u64, access: u32) -> StatusResult<OpenResult>;
    fn validate_state(&self, stateid: StateId) -> StatusResult<()>;
    fn read_state(&self, stateid: StateId, offset: u64, count: u32) -> StatusResult<OpenRead>;
    fn close_state(&self, stateid: StateId) -> StatusResult<StateId>;
    fn renew_client(&self, clientid: u64) -> StatusResult<()>;

    fn parent(&self, id: u64) -> StatusResult<u64> {
        Ok(self.attr(id)?.parent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_ns_errors_map_to_delay() {
        for error in [
            NsError::RateLimited { retry_after: None },
            NsError::Timeout,
            NsError::Network,
        ] {
            assert_eq!(Status::from(&error), Status::Delay);
        }
    }

    #[test]
    fn too_large_maps_to_nfs_resource() {
        assert_eq!(Status::from(&NsError::TooLarge), Status::Resource);
    }
}
