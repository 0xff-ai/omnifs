//! Workspace-standard exact-key singleflight and budgeted deferral.
//!
//! - [`Group`]: block-until-done dedupe (OAuth refresh in [`crate::auth`]).
//! - [`Deferred`]: budgeted wait, detached runner, forget-on-complete (proactive
//!   NFS deferral in `omnifs-nfs`).
//!
//! Both exact dedupe and budgeted deferral are implemented on
//! [`crate::coalesce::Coalesce`]; this module keeps the small caller-facing
//! aliases NFS and auth already import.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;

pub use async_singleflight::Group;

use crate::coalesce::{Coalesce, CoverKey, ResolveOutcome};

/// Outcome of a budgeted [`Deferred::resolve`].
pub type DeferOutcome<V> = ResolveOutcome<V>;

/// Exact-key single-flight with a per-caller wait budget.
///
/// Past the budget the caller gets [`DeferOutcome::Pending`] while a detached
/// runner keeps going to completion. Later resolves start fresh once the slot
/// is released.
pub struct Deferred<K: CoverKey, V> {
    coalesce: Arc<Coalesce<K, V>>,
}

impl<K, V> Deferred<K, V>
where
    K: CoverKey,
    V: Send + Sync + 'static,
{
    pub fn new(rt: Handle) -> Self {
        Self {
            coalesce: Arc::new(Coalesce::with_runtime(rt)),
        }
    }

    /// Wait up to `budget` for `key`'s work. The factory runs once on a detached
    /// runner and is shared with concurrent or retried callers; exceeding the
    /// budget yields [`DeferOutcome::Pending`] while the runner keeps going.
    pub fn resolve<F, Fut>(&self, key: &K, budget: Duration, make: F) -> DeferOutcome<V>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = V> + Send + 'static,
    {
        self.coalesce.resolve(key, budget, make)
    }
}
