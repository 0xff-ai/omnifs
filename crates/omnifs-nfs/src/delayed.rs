//! NFS-local proactive deferral of provider-backed directory listings.
//!
//! `Tree` computes the truthful projection result and may block on cold provider
//! work for as long as it takes. The NFS frontend decides how long an individual
//! RPC handler may wait for that truth before replying `NFS4ERR_DELAY` and
//! letting the client retry. That wait budget is frontend policy `Tree`
//! deliberately does not own.
//!
//! Concurrent RPC dispatch (`server.rs`) already keeps one slow op from
//! head-of-line blocking other calls on the same connection: each RPC runs on
//! its own handler thread and replies carry their own XID. Proactive deferral is
//! about not holding a single `READDIR` reply past the inline budget so the
//! macOS client stays responsive for that call.
//!
//! [`Listings`] implements the proactive path for `READDIR` only via
//! [`omnifs_engine::singleflight::Deferred`]. It runs each listing once per
//! directory as a detached leader, lets a caller wait up to a small budget, and
//! reports [`DeferOutcome::Pending`] (mapped to `NFS4ERR_DELAY`) past it. The
//! task is never cancelled, so a slow listing runs to completion and writes its
//! dirents into `Tree`'s cache; the client's retry then re-resolves and hits that
//! warm cache.
//!
//! This convergence holds only on the success path, which `Tree` caches. An
//! errored listing is not cached, so a slow, persistently failing listing
//! re-defers on every retry until it succeeds or the upstream error maps to a
//! terminal status (see `readdir` in `adapter.rs`). That is why the table backs
//! `READDIR` and not `LOOKUP`: a cold child lookup is not cached, so deferring
//! it would re-run provider work on every retry regardless.
//!
//! This is separate from the reactive `NFS4ERR_DELAY` path in
//! [`Status::from`](crate::export::Status) for [`TreeError`](omnifs_engine::TreeError),
//! which maps transient upstream errors on any op without continuing background
//! work past the reply.
//!
//! There is no result retention here on purpose: every resolve goes through
//! `Tree`, so caching and invalidation stay `Tree`'s job and a completed listing
//! never shadows a later fresh answer. Concurrent and retried callers for the
//! same directory share the one leader, so a directory is fetched once no matter
//! how many retransmits arrive.

use omnifs_core::path::Path;
use omnifs_engine::ListOutcome;
use omnifs_engine::coalesce::{CoalesceKey, CoverKey};
pub(crate) use omnifs_engine::singleflight::{DeferOutcome, Deferred};

use crate::export::Status;

/// The already-mapped terminal result of a deferred listing. The adapter does
/// all `TreeError -> Status` conversion before it reaches the table, so this
/// module never touches protocol state.
pub(crate) type ListResult = Result<ListOutcome, Status>;

/// Identity of a deferred directory listing: which mount, which directory.
#[derive(Clone, Eq, Hash, PartialEq)]
pub(crate) struct Key {
    mount: String,
    path: Path,
}

impl Key {
    pub(crate) fn new(mount: &str, path: &Path) -> Self {
        Self {
            mount: mount.to_string(),
            path: path.clone(),
        }
    }
}

impl CoalesceKey for Key {
    type Id = Self;

    fn exact_id(&self) -> Self {
        self.clone()
    }
}

impl CoverKey for Key {}

/// Per-directory single-flight with a per-caller wait budget for deferred
/// `READDIR` listings.
pub(crate) type Listings = Deferred<Key, ListResult>;
