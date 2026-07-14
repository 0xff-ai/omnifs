//! Test harness surface for provider and engine integration tests.

use std::fmt;
use std::path::PathBuf;
use std::sync::mpsc;

use crate::Runtime;
use crate::callouts::{TestSignal, record_outcome as inner_record};
use crate::log_redaction::{LogUrl as InternalLogUrl, WitHeaders as InternalWitHeaders};
use omnifs_wit::provider::types as wit_types;

pub use crate::BuildError;
pub use crate::effect_apply::{LookupEntry, LookupOutcome};
pub use crate::ops::namespace::{
    ChunkOutcome, DirEntry, DirListing, ListOutcome as NamespaceListOutcome, OpenOutcome,
    ReadBytes, ReadOutcome,
};
pub use crate::runtime::wasm::component_engine;
pub use crate::tree::{PaginationControl, Synthetic, SyntheticContent, probe_live_growth};
pub use crate::{Cursor, Engine, EngineError, GitCloner, HostContext};

/// Stable compiled-component cache shared by test processes.
///
/// Runtime data remains in each fixture's temporary cache directory. Only
/// Wasmtime's content-addressed compilation artifacts are shared here.
#[must_use]
pub fn wasm_cache_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map_or_else(
        || {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join("target/wasm-cache")
        },
        |target_dir| PathBuf::from(target_dir).join("wasm-cache"),
    )
}

pub mod auth {
    use omnifs_auth::OAuthClient;
    pub use omnifs_auth::{AuthBinding, CredentialService, RefreshOutcome};
    use omnifs_workspace::authn::AuthManifest;
    use omnifs_workspace::creds::CredentialStore;
    use omnifs_workspace::mounts::Auth;
    use std::sync::Arc;

    /// Build a dedicated mount binding around a test store and OAuth client.
    pub fn binding_with_store_and_http(
        config: Option<&Auth>,
        manifest: Option<&AuthManifest>,
        provider_name: &str,
        store: Arc<dyn CredentialStore>,
        oauth_http: reqwest_oauth2::Client,
    ) -> Option<Arc<AuthBinding>> {
        let service = Arc::new(CredentialService::new(
            store,
            OAuthClient::from_http_client(oauth_http),
        ));
        crate::auth::binding_from_config(config, manifest, provider_name, &service)
            .expect("build test auth binding")
    }

    /// A credential service over an on-disk store, matching the production
    /// wiring in `MountRuntimes::load`. For harnesses that build a `Runtime`
    /// directly.
    pub fn credential_service_for_file(
        credentials_file: &std::path::Path,
    ) -> std::sync::Arc<CredentialService> {
        crate::auth::credential_service_for_file(credentials_file)
            .expect("build test credential service")
    }
}

/// Load a complete immutable mount snapshot with provider callout capture
/// enabled. This keeps live frontend concurrency fixtures on the same startup
/// construction path as the daemon while exposing the capture option only in
/// the test-support surface.
#[doc(hidden)]
pub fn load_mount_runtimes_for_callout_tests(
    context: HostContext,
    cloner: std::sync::Arc<GitCloner>,
    desired: &omnifs_workspace::mounts::Registry,
    handle: &tokio::runtime::Handle,
) -> Result<crate::MountRuntimes, crate::RegistryError> {
    crate::runtime::registry::MountRuntimes::load_with_options(
        context, cloner, desired, handle, true,
    )
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
pub struct TestOp<'a, T> {
    runtime: &'a Runtime,
    id: u64,
    state: TestOpState<T>,
}

type TestResult<T> = std::result::Result<
    (
        std::result::Result<T, wit_types::ProviderError>,
        wit_types::Effects,
    ),
    EngineError,
>;
type TestReceiver<T> = mpsc::Receiver<TestResult<T>>;

enum TestOpState<T> {
    InProgress,
    WaitingForCallouts {
        callouts: Vec<wit_types::Callout>,
        replies: Vec<tokio::sync::oneshot::Sender<wit_types::CalloutResult>>,
        result_rx: TestReceiver<T>,
    },
    Returned {
        result: std::result::Result<T, wit_types::ProviderError>,
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
}

impl Runtime {
    #[doc(hidden)]
    pub fn start_lookup_child(
        &self,
        parent: &omnifs_core::path::Path,
        name: &omnifs_core::path::Segment,
    ) -> crate::runtime::Result<TestOp<'_, wit_types::LookupChildResult>> {
        let id = self.next_operation_id();
        let parent_text = parent.as_str().to_owned();
        let name_text = name.as_str().to_owned();
        if self.test_callouts.is_some() {
            return TestOp::start_callout(self, id, move |instance| async move {
                instance.lookup_child(id, parent_text, name_text).await
            });
        }
        let transport =
            futures::executor::block_on(self.instance.lookup_child(id, parent_text, name_text))?;
        TestOp::from_transport(self, id, Ok(transport))
    }

    #[doc(hidden)]
    pub fn start_list_children(
        &self,
        path: &omnifs_core::path::Path,
        validator: Option<String>,
        cursor: Option<&wit_types::Cursor>,
    ) -> crate::runtime::Result<TestOp<'_, wit_types::ListChildrenResult>> {
        let id = self.next_operation_id();
        let path_text = path.as_str().to_owned();
        let cursor = cursor.cloned();
        if self.test_callouts.is_some() {
            let callout_path = path_text.clone();
            return TestOp::start_callout(self, id, move |instance| async move {
                instance
                    .list_children(id, callout_path, validator, cursor)
                    .await
            });
        }
        let transport = futures::executor::block_on(
            self.instance
                .list_children(id, path_text, validator, cursor),
        )?;
        TestOp::from_transport(self, id, Ok(transport))
    }

    #[doc(hidden)]
    pub fn start_read_file(
        &self,
        path: &omnifs_core::path::Path,
        content_type: &str,
        cached: Option<wit_types::CanonicalInput>,
    ) -> crate::runtime::Result<TestOp<'_, wit_types::ReadFileOutcome>> {
        let id = self.next_operation_id();
        let path_text = path.as_str().to_owned();
        let content_type = content_type.to_owned();
        if self.test_callouts.is_some() {
            let callout_path = path_text.clone();
            return TestOp::start_callout(self, id, move |instance| async move {
                instance
                    .read_file(id, callout_path, content_type, cached)
                    .await
            });
        }
        let transport = futures::executor::block_on(self.instance.read_file(
            id,
            path_text,
            content_type,
            cached,
        ))?;
        TestOp::from_transport(self, id, Ok(transport))
    }

    #[doc(hidden)]
    pub fn start_event(
        &self,
        event: wit_types::ProviderEvent,
    ) -> crate::runtime::Result<TestOp<'_, ()>> {
        let id = self.next_operation_id();
        if self.test_callouts.is_some() {
            return TestOp::start_callout(self, id, move |instance| async move {
                instance.on_event(id, event).await
            });
        }
        let transport = futures::executor::block_on(self.instance.on_event(id, event))?;
        TestOp::from_transport(self, id, Ok(transport))
    }
}

impl<'a, T> TestOp<'a, T> {
    fn start_callout<F, Fut>(runtime: &'a Runtime, id: u64, make: F) -> crate::runtime::Result<Self>
    where
        F: FnOnce(crate::runtime::instance::Instance) -> Fut + Send + 'static,
        Fut: std::future::Future<
                Output = std::result::Result<
                    (
                        std::result::Result<T, wit_types::ProviderError>,
                        wit_types::Effects,
                    ),
                    EngineError,
                >,
            > + Send
            + 'static,
        T: Send + 'static,
    {
        let instance = runtime.instance.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name(format!("omnifs-test-op-{id}"))
            .spawn(move || {
                let _ = tx.send(futures::executor::block_on(make(instance)));
            })
            .map_err(|e| EngineError::ProviderProtocol(format!("spawn test op: {e}")))?;
        let state = Self::wait_for_progress(runtime, id, rx)?;
        Ok(Self { runtime, id, state })
    }

    fn wait_for_progress(
        runtime: &'a Runtime,
        id: u64,
        result_rx: TestReceiver<T>,
    ) -> crate::runtime::Result<TestOpState<T>> {
        let inbox = runtime.test_callouts.as_ref().ok_or_else(|| {
            EngineError::ProviderProtocol("test callout inbox is not configured".to_string())
        })?;
        loop {
            match result_rx.try_recv() {
                Ok(transport) => {
                    let (result, effects) = transport?;
                    return Ok(TestOpState::Returned {
                        result,
                        effects: Box::new(effects),
                    });
                },
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(EngineError::ProviderProtocol(
                        "provider operation result channel closed".to_string(),
                    ));
                },
                Err(mpsc::TryRecvError::Empty) => {},
            }
            let signal = inbox
                .lock()
                .map_err(|_| {
                    EngineError::ProviderProtocol("test callout receiver poisoned".to_string())
                })?
                .recv_timeout(std::time::Duration::from_millis(10));
            match signal {
                Ok(TestSignal::Callout(first)) => {
                    if first.op_id != id {
                        return Err(EngineError::ProviderProtocol(
                            "test callout operation id mismatch".to_string(),
                        ));
                    }
                    let mut callouts = vec![first.callout];
                    let mut replies = vec![first.reply];
                    loop {
                        let signal = inbox
                            .lock()
                            .map_err(|_| {
                                EngineError::ProviderProtocol(
                                    "test callout receiver poisoned".to_string(),
                                )
                            })?
                            .recv_timeout(std::time::Duration::from_secs(5));
                        match signal {
                            Ok(TestSignal::Callout(next)) => {
                                if next.op_id != id {
                                    return Err(EngineError::ProviderProtocol(
                                        "test callout operation id mismatch".to_string(),
                                    ));
                                }
                                callouts.push(next.callout);
                                replies.push(next.reply);
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
                Ok(TestSignal::Parked) | Err(mpsc::RecvTimeoutError::Timeout) => {},
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(EngineError::ProviderProtocol(
                        "test callout receiver closed".to_string(),
                    ));
                },
            }
        }
    }

    fn from_transport(
        runtime: &'a Runtime,
        id: u64,
        transport: TestResult<T>,
    ) -> crate::runtime::Result<Self> {
        let (result, effects) = transport?;
        Ok(Self {
            runtime,
            id,
            state: TestOpState::Returned {
                result,
                effects: Box::new(effects),
            },
        })
    }

    pub fn is_returned(&self) -> bool {
        matches!(self.state, TestOpState::Returned { .. })
    }
    pub fn result(&self) -> Option<&std::result::Result<T, wit_types::ProviderError>> {
        match &self.state {
            TestOpState::Returned { result, .. } => Some(result),
            _ => None,
        }
    }
    pub fn effects(&self) -> Option<&wit_types::Effects> {
        match &self.state {
            TestOpState::Returned { effects, .. } => Some(effects),
            _ => None,
        }
    }
    pub fn into_result(
        self,
    ) -> crate::runtime::Result<std::result::Result<T, wit_types::ProviderError>> {
        match self.state {
            TestOpState::Returned { result, .. } => Ok(result),
            _ => Err(EngineError::ProviderProtocol(
                "provider operation has not returned".to_string(),
            )),
        }
    }
    pub fn into_result_and_effects(
        self,
    ) -> crate::runtime::Result<(
        std::result::Result<T, wit_types::ProviderError>,
        wit_types::Effects,
    )> {
        match self.state {
            TestOpState::Returned { result, effects } => Ok((result, *effects)),
            _ => Err(EngineError::ProviderProtocol(
                "provider operation has not returned".to_string(),
            )),
        }
    }
    pub fn callouts(&self) -> &[wit_types::Callout] {
        match &self.state {
            TestOpState::WaitingForCallouts { callouts, .. } => callouts,
            _ => &[],
        }
    }
    pub fn is_waiting_for_callouts(&self) -> bool {
        matches!(self.state, TestOpState::WaitingForCallouts { .. })
    }

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
        if replies.len() != results.len() {
            return Err(EngineError::ProviderProtocol(format!(
                "expected {} test callout results, got {}",
                replies.len(),
                results.len()
            )));
        }
        for (reply, result) in replies.into_iter().zip(results) {
            let _ = reply.send(result);
        }
        self.state = Self::wait_for_progress(self.runtime, self.id, result_rx)?;
        Ok(())
    }
}

impl<T: fmt::Debug> fmt::Debug for TestOp<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TestOp")
            .field("id", &self.id)
            .field("state", &self.is_returned())
            .finish()
    }
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
