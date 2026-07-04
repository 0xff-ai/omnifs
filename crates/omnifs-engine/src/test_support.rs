//! Test harness surface for provider and engine integration tests.

use std::fmt;
use std::sync::mpsc;

use crate::Runtime;
use crate::cache::{
    CachedCanonical, Caches, Record, RecordKind, SCHEMA_VERSION, Store, object, view as cache_view,
};
use crate::callouts::{CalloutKind, TestCallout, TestSignal, record_outcome as inner_record};
use crate::log_redaction::{LogUrl as InternalLogUrl, WitHeaders as InternalWitHeaders};
use omnifs_wit::provider::types as wit_types;

pub use crate::BuildError;
pub use crate::effect_apply::{LookupEntry, LookupOutcome};
pub use crate::ops::namespace::{
    ChunkOutcome, DirEntry, DirListing, ListOutcome as NamespaceListOutcome, OpenOutcome,
    ReadBytes, ReadOutcome,
};
pub use crate::ops::op::Op;
pub use crate::runtime::wasm::{component_engine, provider_compiler_strategy};
pub use crate::tree::{PaginationControl, Synthetic, SyntheticContent, probe_live_growth};
pub use crate::{Cursor, Engine, EngineError, GitCloner, HostContext};

pub mod auth {
    pub use crate::auth::{AuthManager, RefreshOutcome};
}

pub mod blob {
    pub use crate::blob::{BlobCache, BlobExecutor, BlobLimits};
}

pub mod capability {
    pub use crate::capability::CapabilityChecker;
}

pub mod clock {
    pub use crate::clock::{DYNAMIC_TTL_MILLIS, now_millis};
}

pub mod http {
    pub use crate::http::HttpStack;
}

pub mod pagination {
    pub use crate::pagination::{MAX_PAGINATION_PAGES, NextPageOutcome};
    pub use crate::tree::synthetic::{IGNORE_CONTENT, is_reserved_provider_leaf};
}

pub mod wit_protocol {
    pub use crate::wit_protocol::*;
}

pub mod wit {
    pub use omnifs_wit::provider::types::*;
}

/// Cache APIs used by integration tests without exposing cache internals as a
/// normal engine surface.
pub mod cache {
    pub use crate::cache::store::{
        BatchRecord, CachedCanonical, Caches, CanonicalBatchEntry, Handle, Key, Record, RecordKind,
        SCHEMA_VERSION, Store,
    };
    pub use crate::cache::{object, view};
}

/// Test operation driver used by provider integration tests that need to
/// inspect and answer captured host imports. This is not the provider runtime
/// protocol: production operations await WIT async host imports directly.
#[doc(hidden)]
pub struct TestOp<'a> {
    runtime: &'a Runtime,
    op: Op,
    id: u64,
    op_gen: u64,
    state: TestOpState,
}

enum TestOpState {
    InProgress,
    WaitingForCallouts {
        callouts: Vec<wit_types::Callout>,
        replies: Vec<tokio::sync::oneshot::Sender<wit_types::CalloutResult>>,
        result_rx: mpsc::Receiver<std::result::Result<wit_types::ProviderReturn, EngineError>>,
    },
    Returned {
        result: Box<wit_types::OpResult>,
        effects: Box<wit_types::Effects>,
    },
}

/// Test-only handle to one captured provider callout awaiting its answer. See
/// [`Engine::try_recv_test_callout`].
#[doc(hidden)]
pub struct PendingTestCallout {
    op_id: u64,
    callout: wit_types::Callout,
    reply: tokio::sync::oneshot::Sender<wit_types::CalloutResult>,
}

impl PendingTestCallout {
    #[doc(hidden)]
    #[must_use]
    pub fn op_id(&self) -> u64 {
        self.op_id
    }

    #[doc(hidden)]
    #[must_use]
    pub fn callout(&self) -> &wit_types::Callout {
        &self.callout
    }

    /// Resume the suspended provider future with `result`.
    #[doc(hidden)]
    pub fn answer(self, result: wit_types::CalloutResult) {
        let _ = self.reply.send(result);
    }
}

impl Runtime {
    /// Non-blocking receive of the next captured provider callout, if one has
    /// been issued and not yet answered. Only yields values on runtimes built
    /// with [`Engine::new_for_callout_tests`]; returns `None` otherwise or when
    /// no callout is pending. Lets a concurrency test observe that two ops are
    /// suspended on host imports at the same instant before answering either.
    #[doc(hidden)]
    pub fn try_recv_test_callout(&self) -> Option<PendingTestCallout> {
        let inbox = self.test_callouts.as_ref()?;
        let guard = inbox.lock().ok()?;
        loop {
            match guard.try_recv() {
                Ok(TestSignal::Callout(callout)) => {
                    return Some(PendingTestCallout {
                        op_id: callout.op_id,
                        callout: callout.callout,
                        reply: callout.reply,
                    });
                },
                // Idle-executor markers are not callouts; skip them.
                Ok(TestSignal::Parked) => {},
                Err(_) => return None,
            }
        }
    }

    /// Synchronous test entry: blocks the caller until the operation returns or
    /// suspends on captured callouts. Production code drives ops through the
    /// async [`Engine::run_op`] path instead; this exists for the provider
    /// integration harness (`omnifs-itest`).
    #[doc(hidden)]
    pub fn start_op(&self, op: Op) -> crate::runtime::Result<TestOp<'_>> {
        let op_gen = self.cache.current_generation();
        let id = self.next_operation_id();
        if self.test_callouts.is_some() {
            return TestOp::start_callout_test(self, op, id, op_gen);
        }
        let ret = futures::executor::block_on(self.instance.start_op(op.clone(), id))?;
        TestOp::from_return(self, op, id, op_gen, ret)
    }
}

impl<'a> TestOp<'a> {
    fn start_callout_test(
        runtime: &'a Runtime,
        op: Op,
        id: u64,
        op_gen: u64,
    ) -> crate::runtime::Result<Self> {
        let instance = runtime.instance.clone();
        let op_for_task = op.clone();
        let (result_tx, result_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name(format!("omnifs-test-op-{id}"))
            .spawn(move || {
                let result = futures::executor::block_on(instance.start_op(op_for_task, id));
                let _ = result_tx.send(result);
            })
            .map_err(|error| EngineError::ProviderProtocol(format!("spawn test op: {error}")))?;

        let state = Self::wait_for_progress(runtime, &op, id, op_gen, result_rx)?;
        Ok(Self {
            runtime,
            op,
            id,
            op_gen,
            state,
        })
    }

    fn from_return(
        runtime: &'a Runtime,
        op: Op,
        id: u64,
        op_gen: u64,
        ret: wit_types::ProviderReturn,
    ) -> crate::runtime::Result<Self> {
        let state = Self::returned_state(runtime, &op, op_gen, ret)?;
        Ok(Self {
            runtime,
            op,
            id,
            op_gen,
            state,
        })
    }

    /// Safety net for a `Parked` marker that never arrives while a callout
    /// burst is draining. The marker, not this timeout, closes a burst; the
    /// window is wide enough that only a genuine lost signal trips it.
    const BURST_WATCHDOG: std::time::Duration = std::time::Duration::from_secs(5);

    fn wait_for_progress(
        runtime: &Runtime,
        op: &Op,
        id: u64,
        op_gen: u64,
        result_rx: mpsc::Receiver<std::result::Result<wit_types::ProviderReturn, EngineError>>,
    ) -> crate::runtime::Result<TestOpState> {
        let inbox = runtime.test_callouts.as_ref().ok_or_else(|| {
            EngineError::ProviderProtocol("test callout inbox is not configured".to_string())
        })?;
        let recv_signal = |timeout| {
            inbox
                .lock()
                .expect("test callout receiver poisoned")
                .recv_timeout(timeout)
        };
        loop {
            match result_rx.try_recv() {
                Ok(ret) => return Self::returned_state(runtime, op, op_gen, ret?),
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(EngineError::ProviderProtocol(
                        "provider operation result channel closed".to_string(),
                    ));
                },
                Err(mpsc::TryRecvError::Empty) => {},
            }

            match recv_signal(std::time::Duration::from_millis(10)) {
                Ok(TestSignal::Callout(first)) => {
                    let mut callouts = Vec::new();
                    let mut replies = Vec::new();
                    Self::push_test_callout(id, first, &mut callouts, &mut replies)?;
                    // The instance's single-threaded executor enqueues every
                    // callout of this round before it can park, so a `Parked`
                    // marker arrives in FIFO order right after the last one.
                    loop {
                        match recv_signal(Self::BURST_WATCHDOG) {
                            Ok(TestSignal::Callout(next)) => {
                                Self::push_test_callout(id, next, &mut callouts, &mut replies)?;
                            },
                            Ok(TestSignal::Parked) | Err(mpsc::RecvTimeoutError::Timeout) => break,
                            Err(mpsc::RecvTimeoutError::Disconnected) => {
                                return Err(EngineError::ProviderProtocol(
                                    "test callout receiver closed".to_string(),
                                ));
                            },
                        }
                    }
                    return Ok(TestOpState::WaitingForCallouts {
                        callouts,
                        replies,
                        result_rx,
                    });
                },
                // A park with no callout in flight, or a poll timeout: loop to
                // re-check the result channel.
                Ok(TestSignal::Parked) | Err(mpsc::RecvTimeoutError::Timeout) => {},
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(EngineError::ProviderProtocol(
                        "test callout receiver closed".to_string(),
                    ));
                },
            }
        }
    }

    fn push_test_callout(
        id: u64,
        test_callout: TestCallout,
        callouts: &mut Vec<wit_types::Callout>,
        replies: &mut Vec<tokio::sync::oneshot::Sender<wit_types::CalloutResult>>,
    ) -> crate::runtime::Result<()> {
        if test_callout.op_id != id {
            return Err(EngineError::ProviderProtocol(format!(
                "test callout for operation {} received while driving operation {id}",
                test_callout.op_id
            )));
        }
        callouts.push(test_callout.callout);
        replies.push(test_callout.reply);
        Ok(())
    }

    fn returned_state(
        runtime: &Runtime,
        op: &Op,
        op_gen: u64,
        ret: wit_types::ProviderReturn,
    ) -> crate::runtime::Result<TestOpState> {
        let effects = ret.effects.clone();
        let result = runtime.finish_provider_return(op, ret, op_gen)?;
        runtime.note_returned_result(&result);
        Ok(TestOpState::Returned {
            result: Box::new(result),
            effects: Box::new(effects),
        })
    }

    pub fn callouts(&self) -> &[wit_types::Callout] {
        match &self.state {
            TestOpState::WaitingForCallouts { callouts, .. } => callouts,
            TestOpState::InProgress | TestOpState::Returned { .. } => &[],
        }
    }

    pub fn is_waiting_for_callouts(&self) -> bool {
        matches!(self.state, TestOpState::WaitingForCallouts { .. })
    }

    pub fn is_returned(&self) -> bool {
        matches!(self.state, TestOpState::Returned { .. })
    }

    #[allow(clippy::needless_pass_by_value)]
    pub fn answer_callouts(
        &mut self,
        results: Vec<wit_types::CalloutResult>,
    ) -> crate::runtime::Result<()> {
        let state = std::mem::replace(&mut self.state, TestOpState::InProgress);
        let TestOpState::WaitingForCallouts {
            replies, result_rx, ..
        } = state
        else {
            return Err(EngineError::ProviderProtocol(
                "provider operation is not waiting on test callouts".to_string(),
            ));
        };
        if results.len() != replies.len() {
            return Err(EngineError::ProviderProtocol(format!(
                "expected {} test callout results, got {}",
                replies.len(),
                results.len()
            )));
        }
        for (reply, result) in replies.into_iter().zip(results) {
            let _ = reply.send(result);
        }
        self.state =
            Self::wait_for_progress(self.runtime, &self.op, self.id, self.op_gen, result_rx)?;
        Ok(())
    }

    pub fn into_result(self) -> crate::runtime::Result<wit_types::OpResult> {
        match self.state {
            TestOpState::Returned { result, .. } => Ok(*result),
            TestOpState::WaitingForCallouts { .. } | TestOpState::InProgress => Err(
                EngineError::ProviderProtocol("provider operation has not returned".to_string()),
            ),
        }
    }

    pub fn result(&self) -> Option<&wit_types::OpResult> {
        match &self.state {
            TestOpState::Returned { result, .. } => Some(result.as_ref()),
            TestOpState::WaitingForCallouts { .. } | TestOpState::InProgress => None,
        }
    }

    pub fn effects(&self) -> Option<&wit_types::Effects> {
        match &self.state {
            TestOpState::Returned { effects, .. } => Some(effects.as_ref()),
            TestOpState::WaitingForCallouts { .. } | TestOpState::InProgress => None,
        }
    }

    pub fn into_result_and_effects(
        self,
    ) -> crate::runtime::Result<(wit_types::OpResult, wit_types::Effects)> {
        match self.state {
            TestOpState::Returned { result, effects } => Ok((*result, *effects)),
            TestOpState::WaitingForCallouts { .. } | TestOpState::InProgress => Err(
                EngineError::ProviderProtocol("provider operation has not returned".to_string()),
            ),
        }
    }
}

impl fmt::Debug for TestOp<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = f.debug_struct("TestOp");
        debug.field("id", &self.id).field("op", &self.op);
        match &self.state {
            TestOpState::InProgress => {
                debug.field("state", &"in_progress");
            },
            TestOpState::WaitingForCallouts { callouts, .. } => {
                debug.field("state", &"waiting-for-callouts");
                debug.field("callouts", callouts);
            },
            TestOpState::Returned { result, effects } => {
                debug.field("state", &"returned");
                debug.field("result", result);
                debug.field("effects", effects);
            },
        }
        debug.finish()
    }
}

/// Stable kind labels used by the outer dispatch span. Kept in lockstep
/// with the internal `CalloutKind` strum labels.
pub fn kind_label(callout: &wit_types::Callout) -> &'static str {
    CalloutKind::of(callout).into()
}

/// Public re-display wrapper for redacting URLs in log output.
pub struct LogUrl<'a>(pub &'a str);

impl fmt::Display for LogUrl<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        InternalLogUrl(self.0).fmt(f)
    }
}

/// Public re-display wrapper for redacting WIT headers in log output.
pub struct WitHeaders<'a>(pub &'a [wit_types::Header]);

impl fmt::Display for WitHeaders<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        InternalWitHeaders(self.0).fmt(f)
    }
}

/// Records the outcome fields on `Span::current()` for the given
/// callout result, exactly as the production executor methods do.
pub fn record_outcome(result: &wit_types::CalloutResult) {
    inner_record(result);
}

#[allow(dead_code)]
fn _cache_types_are_part_of_test_support() -> Option<(
    CachedCanonical,
    Caches,
    Record,
    RecordKind,
    Store,
    object::Cache,
    cache_view::Cache,
)> {
    let _ = SCHEMA_VERSION;
    None
}
