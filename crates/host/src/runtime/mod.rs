//! Callout runtime: WASM provider execution and callout handling.
//!
//! Manages the Wasmtime store, routes provider callouts to host
//! implementations (HTTP, Git), and drives async continuations.

pub mod activity;
pub mod archive;
pub mod blob;
mod browse_pipeline;
pub mod capability;
pub mod cloner;
pub mod correlation;
pub mod coverage;
pub mod executor;
pub mod git;
pub mod http_headers;
pub mod inflight;
mod invalidation;
pub mod manifest;
pub(crate) mod sandbox;
pub mod tools;
pub mod tree_refs;
pub(crate) mod wasm;

use crate::Provider;
use crate::auth::AuthManager;
use crate::cache;
use crate::cache::blobs::BlobCache;
use crate::cache::l2::Cache as L2Cache;
use crate::cache::{CacheRecord, Key, RecordKind};
use crate::config::InstanceConfig;
use crate::config::schema;
use crate::omnifs::provider::log::Host as LogHost;
use crate::omnifs::provider::types::{self as wit_types, Host as TypesHost};
use crate::runtime::activity::ActivityTable;
use crate::runtime::archive::ArchiveExecutor;
use crate::runtime::blob::{BlobExecutor, BlobLimits};
use crate::runtime::capability::{CapabilityChecker, CapabilityGrants};
use crate::runtime::cloner::GitCloner;
use crate::runtime::correlation::CorrelationTracker;
use crate::runtime::executor::{CalloutResponse, ErrorKind, HttpExecutor};
use crate::runtime::inflight::InFlight;
use crate::runtime::invalidation::InvalidationState;
use crate::runtime::manifest::{DeclaredHandler, read_declared_handlers_from_wasm};
use crate::runtime::tools::archive::{ArchiveExtractorComponent, ArchiveFormat};
use crate::runtime::tree_refs::TreeRefs;
#[cfg(target_os = "linux")]
use fuser::Notifier;
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use wasmtime::component::{Component, HasData, Linker, ResourceTable};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

const ACTIVITY_TTL: std::time::Duration = std::time::Duration::from_secs(600);

/// FUSE notifier handle (only available on Linux with FUSE support).
#[cfg(target_os = "linux")]
pub type NotifierHandle = Arc<Mutex<Option<Notifier>>>;

/// FUSE notifier handle (stub for non-Linux platforms).
#[cfg(not(target_os = "linux"))]
pub type NotifierHandle = Arc<Mutex<Option<()>>>;

/// Runtime for executing WASM provider components.
///
/// Manages the Wasmtime store, routes callouts, and handles async
/// continuations with correlation tracking.
pub struct CalloutRuntime {
    store: Mutex<wasmtime::Store<HostState>>,
    bindings: Provider,
    config_bytes: Vec<u8>,
    correlations: CorrelationTracker,
    http: HttpExecutor,
    git: git::GitExecutor,
    blob: BlobExecutor,
    archive: Arc<ArchiveExecutor>,
    blob_cache: Arc<BlobCache>,
    trees: Arc<TreeRefs>,
    l2: Option<L2Cache>,
    invalidation: InvalidationState,
    activity_table: Mutex<ActivityTable>,
    declared_handlers: Vec<DeclaredHandler>,
    inflight: InFlight,
}

struct HostState {
    wasi: WasiCtx,
    table: ResourceTable,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl HasData for HostState {
    type Data<'a> = &'a mut HostState;
}

impl TypesHost for HostState {}
impl LogHost for HostState {
    fn log(&mut self, entry: wit_types::LogEntry) {
        match entry.level {
            wit_types::LogLevel::Trace => tracing::trace!("{}", entry.message),
            wit_types::LogLevel::Debug => tracing::debug!("{}", entry.message),
            wit_types::LogLevel::Info => tracing::info!("{}", entry.message),
            wit_types::LogLevel::Warn => tracing::warn!("{}", entry.message),
            wit_types::LogLevel::Error => tracing::error!("{}", entry.message),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("wasmtime error: {0}")]
    Wasmtime(#[from] wasmtime::Error),
    #[error("failed to build HTTP client: {0}")]
    HttpClient(#[from] reqwest::Error),
    #[error("provider returned error: {0}")]
    ProviderError(String),
    #[error("{0}")]
    InvalidConfig(String),
    #[error("unexpected response type")]
    UnexpectedResponse,
}

type Result<T> = std::result::Result<T, RuntimeError>;

impl CalloutRuntime {
    pub fn new(
        engine: &wasmtime::Engine,
        wasm_path: &Path,
        config: &InstanceConfig,
        cloner: Arc<GitCloner>,
        cache_dir: &Path,
        mount_name: &str,
        extractor: Arc<ArchiveExtractorComponent>,
    ) -> Result<Self> {
        let mut linker = Linker::<HostState>::new(engine);

        wasm::add_wasi_to_linker::<HostState>(&mut linker)?;
        Provider::add_to_linker::<HostState, HostState>(&mut linker, |state| state)?;

        let component = Component::from_file(engine, wasm_path)?;
        let wasi = WasiCtxBuilder::new().build();
        let mut store = wasmtime::Store::new(
            engine,
            HostState {
                wasi,
                table: ResourceTable::new(),
            },
        );

        let bindings = Provider::instantiate(&mut store, &component, &linker)?;

        // Query the provider's declared capabilities and incorporate needs_git.
        let provider_caps = bindings
            .omnifs_provider_lifecycle()
            .call_capabilities(&mut store)?;

        let grants = build_grants(config, provider_caps.needs_git);
        let capability = Arc::new(CapabilityChecker::new(grants));

        // Validate instance config against the provider's declared schema.
        let wit_schema = bindings
            .omnifs_provider_lifecycle()
            .call_get_config_schema(&mut store)?;
        validate_instance_config(wit_schema.as_deref(), config, mount_name)?;
        let config_bytes = config.config_bytes();
        let auth = if config.auth.is_empty() {
            Arc::new(AuthManager::none())
        } else {
            Arc::new(
                AuthManager::from_configs(&config.auth)
                    .map_err(|e| RuntimeError::ProviderError(format!("auth config error: {e}")))?,
            )
        };

        let trees = Arc::new(TreeRefs::new());
        let git = git::GitExecutor::new(cloner, capability.clone(), trees.clone());

        let provider_cache_root = cache_dir.join("providers").join(mount_name);
        let blob_cache_dir = provider_cache_root.join("blobs");
        let archive_root = provider_cache_root.join("archives");
        if let Err(e) = std::fs::create_dir_all(&blob_cache_dir) {
            tracing::warn!(
                dir = %blob_cache_dir.display(),
                error = %e,
                "failed to create blob cache dir; fetch-blob will fail until resolved"
            );
        }
        if let Err(e) = std::fs::create_dir_all(&archive_root) {
            tracing::warn!(
                dir = %archive_root.display(),
                error = %e,
                "failed to create archive extract dir; open-archive will fail until resolved"
            );
        }
        let blob_cache = Arc::new(BlobCache::new(blob_cache_dir));
        let archive = Arc::new(ArchiveExecutor::new(
            blob_cache.clone(),
            trees.clone(),
            archive_root,
            extractor,
        ));

        let l2 = {
            let db_path = provider_cache_root.join("browse.redb");
            match L2Cache::open(&db_path) {
                Ok(cache) => Some(cache),
                Err(e) => {
                    tracing::warn!(mount = mount_name, error = %e, "failed to open L2 browse cache");
                    None
                },
            }
        };
        let declared_handlers =
            read_declared_handlers_from_wasm(wasm_path).map_err(RuntimeError::InvalidConfig)?;

        let blob_limits = blob_limits_from_config(config);
        let blob = BlobExecutor::new(
            auth.clone(),
            capability.clone(),
            blob_cache.clone(),
            blob_limits,
        )?;
        Ok(Self {
            store: Mutex::new(store),
            bindings,
            config_bytes,
            correlations: CorrelationTracker::new(),
            http: HttpExecutor::new(auth, capability)?,
            git,
            blob,
            archive,
            blob_cache,
            trees,
            l2,
            invalidation: InvalidationState::default(),
            activity_table: Mutex::new(ActivityTable::new(ACTIVITY_TTL)),
            declared_handlers,
            inflight: InFlight::new(),
        })
    }

    pub fn initialize(&self) -> Result<wit_types::OpResult> {
        let response = {
            let mut store = self.store.lock();
            self.bindings
                .omnifs_provider_lifecycle()
                .call_initialize(&mut *store, &self.config_bytes)?
        };
        Self::resolve_response_sync(response)
    }

    pub fn shutdown(&self) -> Result<()> {
        let mut store = self.store.lock();
        self.bindings
            .omnifs_provider_lifecycle()
            .call_shutdown(&mut *store)?;
        Ok(())
    }

    pub fn config_schema(&self) -> Result<Option<String>> {
        let mut store = self.store.lock();
        Ok(self
            .bindings
            .omnifs_provider_lifecycle()
            .call_get_config_schema(&mut *store)?)
    }

    pub fn capabilities(&self) -> Result<wit_types::RequestedCapabilities> {
        let mut store = self.store.lock();
        Ok(self
            .bindings
            .omnifs_provider_lifecycle()
            .call_capabilities(&mut *store)?)
    }

    pub fn call_close_file(&self, handle: u64) -> Result<()> {
        let mut store = self.store.lock();
        self.bindings
            .omnifs_provider_browse()
            .call_close_file(&mut *store, handle)?;
        Ok(())
    }

    pub fn cache_get(&self, path: &str, kind: RecordKind) -> Option<CacheRecord> {
        self.l2.as_ref()?.get(&Key::new(path, kind)).ok().flatten()
    }

    pub fn cache_put(&self, path: &str, kind: RecordKind, record: &CacheRecord) {
        if let Some(ref l2) = self.l2
            && let Err(e) = l2.put(&Key::new(path, kind), record)
        {
            tracing::debug!(path, error = %e, "L2 cache put failed");
        }
    }

    pub fn cache_put_batch(&self, records: &[(String, RecordKind, CacheRecord)]) {
        if let Some(ref l2) = self.l2
            && let Err(e) = l2.put_batch(records)
        {
            tracing::debug!(error = %e, "L2 cache batch put failed");
        }
    }

    #[doc(hidden)]
    pub fn __active_path_sets(&self) -> Vec<wit_types::ActivePathSet> {
        self.activity_table.lock().active_path_sets()
    }

    fn push_projected_file(
        batch: &mut Vec<(String, RecordKind, CacheRecord)>,
        file_path: &str,
        content: &[u8],
    ) {
        use cache::{AttrPayload, EntryKindCache, LookupPayload};

        let file_size = u64::try_from(content.len()).unwrap_or(u64::MAX);
        batch.push((
            file_path.to_string(),
            RecordKind::File,
            CacheRecord::new(RecordKind::File, content.to_vec()),
        ));

        let pf_lookup = LookupPayload::Positive {
            kind: EntryKindCache::File,
            size: file_size,
        };
        if let Some(payload) = pf_lookup.serialize() {
            batch.push((
                file_path.to_string(),
                RecordKind::Lookup,
                CacheRecord::new(RecordKind::Lookup, payload),
            ));
        }

        let pf_attr = AttrPayload {
            kind: EntryKindCache::File,
            size: file_size,
        };
        if let Some(payload) = pf_attr.serialize() {
            batch.push((
                file_path.to_string(),
                RecordKind::Attr,
                CacheRecord::new(RecordKind::Attr, payload),
            ));
        }
    }

    fn push_preloaded_entry(
        batch: &mut Vec<(String, RecordKind, CacheRecord)>,
        entry: &wit_types::PreloadedEntry,
    ) {
        use cache::{AttrPayload, EntryKindCache, LookupPayload};

        let kind = EntryKindCache::from(entry.kind);
        let size = entry.size.unwrap_or(0);
        let lookup = LookupPayload::Positive { kind, size };
        if let Some(payload) = lookup.serialize() {
            batch.push((
                entry.path.clone(),
                RecordKind::Lookup,
                CacheRecord::new(RecordKind::Lookup, payload),
            ));
        }

        let attr = AttrPayload { kind, size };
        if let Some(payload) = attr.serialize() {
            batch.push((
                entry.path.clone(),
                RecordKind::Attr,
                CacheRecord::new(RecordKind::Attr, payload),
            ));
        }
    }

    pub(super) fn apply_preloads(&self, items: &[wit_types::PreloadItem]) {
        use cache::{DirentRecord, DirentsPayload, EntryKindCache};

        if items.is_empty() {
            return;
        }
        let mut batch = Vec::new();
        let mut child_records: BTreeMap<String, BTreeMap<String, DirentRecord>> = BTreeMap::new();
        let mut preloaded_dirs = Vec::new();
        for item in items {
            match item {
                wit_types::PreloadItem::File(file) => {
                    Self::push_projected_file(&mut batch, &file.path, &file.content);
                    Self::record_preload_child(
                        &mut child_records,
                        &file.path,
                        EntryKindCache::File,
                        u64::try_from(file.content.len()).unwrap_or(u64::MAX),
                    );
                },
                wit_types::PreloadItem::Entry(entry) => {
                    Self::push_preloaded_entry(&mut batch, entry);
                    let kind = EntryKindCache::from(entry.kind);
                    if matches!(kind, EntryKindCache::Directory) {
                        preloaded_dirs.push(entry.path.clone());
                    }
                    Self::record_preload_child(
                        &mut child_records,
                        &entry.path,
                        kind,
                        entry.size.unwrap_or(0),
                    );
                },
            }
        }
        for path in preloaded_dirs {
            let Some(children) = child_records.remove(&path) else {
                continue;
            };
            let dirents = DirentsPayload {
                entries: children.into_values().collect(),
                exhaustive: false,
            };
            if let Some(payload) = dirents.serialize() {
                batch.push((
                    path,
                    RecordKind::Dirents,
                    CacheRecord::new(RecordKind::Dirents, payload),
                ));
            }
        }

        if !batch.is_empty() {
            tracing::debug!(
                target: "omnifs_cache",
                kind = "preload",
                count = batch.len(),
                "caching preloads"
            );
            self.cache_put_batch(&batch);
        }
    }

    fn record_preload_child(
        child_records: &mut BTreeMap<String, BTreeMap<String, cache::DirentRecord>>,
        path: &str,
        kind: cache::EntryKindCache,
        size: u64,
    ) {
        let Some((parent, name)) = path.rsplit_once('/') else {
            return;
        };
        if parent.is_empty() || name.is_empty() {
            return;
        }
        child_records.entry(parent.to_string()).or_default().insert(
            name.to_string(),
            cache::DirentRecord {
                name: name.to_string(),
                kind,
                size,
            },
        );
    }

    pub(super) fn apply_event_outcome(&self, outcome: &wit_types::EventOutcome) {
        for path in &outcome.invalidate_paths {
            self.cache_delete_path(path);
            self.invalidation.record_path(path.clone());
        }
        for prefix in &outcome.invalidate_prefixes {
            self.cache_delete_prefix(prefix);
            self.invalidation.record_prefix(prefix.clone());
        }
    }

    pub async fn call_timer_tick(&self) -> Result<wit_types::OpResult> {
        let id = self.correlations.allocate();
        let active_paths = self.activity_table.lock().active_path_sets();

        let response = {
            let mut store = self.store.lock();
            self.bindings.omnifs_provider_notify().call_on_event(
                &mut *store,
                id,
                &wit_types::ProviderEvent::TimerTick(wit_types::TimerTickContext { active_paths }),
            )?
        };

        self.drive_callouts(id, response).await
    }

    /// Drive a provider call to completion.
    ///
    /// Applies terminal-embedded side effects (dir-listing preloads,
    /// event-outcome invalidations) at the response boundary, before
    /// handing the terminal back to the caller. Cases:
    /// - `terminal = Some, callouts = []` → apply boundary, return terminal.
    /// - `terminal = Some, callouts = [..]` → apply boundary, run trailing
    ///   callouts, discard their results, return terminal.
    /// - `terminal = None, callouts = [..]` → execute the batch, resume
    ///   the guest with positionally aligned results, loop.
    /// - `terminal = None, callouts = []` → protocol error.
    async fn drive_callouts(
        &self,
        id: u64,
        mut response: wit_types::ProviderReturn,
    ) -> Result<wit_types::OpResult> {
        loop {
            let callouts = std::mem::take(&mut response.callouts);
            match response.terminal.take() {
                Some(terminal) => {
                    self.apply_terminal_boundary(&terminal);
                    if !callouts.is_empty() {
                        let _ = self.execute_batch(&callouts).await;
                    }
                    return Ok(terminal);
                },
                None if callouts.is_empty() => {
                    return Err(RuntimeError::ProviderError(
                        "provider returned empty response".into(),
                    ));
                },
                None => {
                    let results = self.execute_batch(&callouts).await;
                    let mut store = self.store.lock();
                    response = self.bindings.omnifs_provider_resume().call_resume(
                        &mut *store,
                        id,
                        &results,
                    )?;
                },
            }
        }
    }

    fn apply_terminal_boundary(&self, terminal: &wit_types::OpResult) {
        match terminal {
            wit_types::OpResult::List(wit_types::ListResult::Entries(listing)) => {
                self.apply_preloads(&listing.preload);
            },
            wit_types::OpResult::Lookup(wit_types::LookupResult::Entry(entry)) => {
                // Directory lookups that hit a dir handler can stage
                // preloads for the looked-up child's contents; surface
                // them here so a subsequent FUSE walk observes a warm
                // cache just like it would after an explicit listing.
                self.apply_preloads(&entry.preload);
            },
            wit_types::OpResult::Event(outcome) => {
                self.apply_event_outcome(outcome);
            },
            _ => {},
        }
    }

    /// Resolve a tree-ref handle to a real filesystem path.
    /// Works for both git clones and extracted archives — they share a
    /// single tree registry, so a `tree-ref` is unambiguous.
    pub fn resolve_tree_ref(&self, tree_ref: u64) -> Option<PathBuf> {
        self.trees.resolve(tree_ref)
    }

    /// Read the full bytes of a stored blob. Used by the FUSE read path
    /// when a `read-file` terminal returns blob-backed file content.
    pub fn read_blob_full(&self, blob_id: u64) -> Result<Vec<u8>> {
        let record = self
            .blob_cache
            .lookup_by_id(blob_id)
            .ok_or_else(|| RuntimeError::ProviderError(format!("blob {blob_id} not found")))?;
        let path = self.blob_cache.blob_path(&record.cache_key);
        std::fs::read(path)
            .map_err(|e| RuntimeError::ProviderError(format!("read blob {blob_id}: {e}")))
    }

    fn resolve_response_sync(response: wit_types::ProviderReturn) -> Result<wit_types::OpResult> {
        if !response.callouts.is_empty() {
            return Err(RuntimeError::ProviderError(
                "initialize must not yield callouts".into(),
            ));
        }
        response
            .terminal
            .ok_or_else(|| RuntimeError::ProviderError("initialize returned empty response".into()))
    }

    async fn execute_single_callout(
        &self,
        callout: &wit_types::Callout,
    ) -> wit_types::CalloutResult {
        match callout {
            wit_types::Callout::Fetch(req) => {
                let headers: Vec<(String, String)> = req
                    .headers
                    .iter()
                    .map(|h| (h.name.clone(), h.value.clone()))
                    .collect();
                let resp = self
                    .http
                    .execute_fetch(&req.method, &req.url, &headers, req.body.as_deref())
                    .await;
                HttpCalloutResponse(resp).into()
            },
            wit_types::Callout::GitOpenRepo(req) => {
                GitOpenResponse(self.git.open_repo(&req.cache_key, &req.clone_url)).into()
            },
            wit_types::Callout::FetchBlob(req) => {
                let headers: Vec<(String, String)> = req
                    .headers
                    .iter()
                    .map(|h| (h.name.clone(), h.value.clone()))
                    .collect();
                let resp = self
                    .blob
                    .fetch_blob(
                        &req.method,
                        &req.url,
                        &headers,
                        req.body.as_deref(),
                        &req.cache_key,
                    )
                    .await;
                BlobFetchResponse(resp).into()
            },
            wit_types::Callout::OpenArchive(req) => {
                let format = match req.format {
                    wit_types::ArchiveFormat::TarGz => ArchiveFormat::TarGz,
                    wit_types::ArchiveFormat::Tar => ArchiveFormat::Tar,
                    wit_types::ArchiveFormat::Zip => ArchiveFormat::Zip,
                };
                // Wasm extraction is sync and CPU-bound; run it on the
                // blocking pool so it doesn't park the tokio reactor
                // thread under big tarballs.
                let archive = Arc::clone(&self.archive);
                let blob = req.blob;
                let strip = req.strip_prefix.clone();
                let resp = tokio::task::spawn_blocking(move || {
                    archive.open_archive(blob, format, strip.as_deref())
                })
                .await
                .unwrap_or_else(|join_err| CalloutResponse::Error {
                    kind: ErrorKind::Internal,
                    message: format!("extract task join: {join_err}"),
                    retryable: false,
                });
                ArchiveOpenResponse(resp).into()
            },
            wit_types::Callout::ReadBlob(req) => {
                BlobReadResponse(self.blob.read_blob(req.blob, req.offset, req.len)).into()
            },
            _ => wit_types::CalloutResult::CalloutError(wit_types::CalloutError {
                kind: wit_types::ErrorKind::Internal,
                message: "callout type not yet implemented".to_string(),
                retryable: false,
            }),
        }
    }

    /// Runs every callout in `callouts` concurrently and returns their
    /// outcomes.
    ///
    /// The returned vector is positionally aligned with `callouts`:
    /// outcome `i` is the result of callout `i`. The SDK's `join_all`
    /// pops outcomes from a FIFO queue in yield order, so this ordering
    /// is load-bearing — breaking it would feed results into the wrong
    /// awaiting future. `futures::future::join_all` preserves input
    /// order regardless of completion order, which is exactly what the
    /// guest expects.
    async fn execute_batch(
        &self,
        callouts: &[wit_types::Callout],
    ) -> Vec<wit_types::CalloutResult> {
        let futures: Vec<_> = callouts
            .iter()
            .map(|callout| self.execute_single_callout(callout))
            .collect();
        futures::future::join_all(futures).await
    }
}

fn build_grants(config: &InstanceConfig, needs_git: bool) -> CapabilityGrants {
    let caps = config.capabilities.as_ref();
    CapabilityGrants {
        domains: caps.and_then(|c| c.domains.clone()).unwrap_or_default(),
        git_repos: caps.and_then(|c| c.git_repos.clone()).unwrap_or_default(),
        max_memory_mb: caps.and_then(|c| c.max_memory_mb).unwrap_or(64),
        needs_git,
    }
}

fn blob_limits_from_config(config: &InstanceConfig) -> BlobLimits {
    let defaults = BlobLimits::default();
    let caps = config.capabilities.as_ref();
    BlobLimits {
        max_fetch_blob_bytes: caps
            .and_then(|c| c.max_fetch_blob_bytes)
            .unwrap_or(defaults.max_fetch_blob_bytes),
        max_read_blob_bytes: caps
            .and_then(|c| c.max_read_blob_bytes)
            .unwrap_or(defaults.max_read_blob_bytes),
    }
}

fn validate_instance_config(
    schema_json: Option<&str>,
    config: &InstanceConfig,
    mount_name: &str,
) -> Result<()> {
    let Some(schema_json) = schema_json else {
        return Ok(());
    };

    let empty_config = serde_json::Value::Object(serde_json::Map::new());
    let config_value = config.config_raw.as_ref().unwrap_or(&empty_config);
    match schema::validate_config(schema_json, config_value) {
        Ok(()) => Ok(()),
        Err(schema::SchemaError::Validation(error)) => Err(RuntimeError::InvalidConfig(format!(
            "config for mount {mount_name} failed validation: {error}"
        ))),
        Err(schema::SchemaError::InvalidSchema(error)) => Err(RuntimeError::ProviderError(
            format!("provider config schema for mount {mount_name} is invalid: {error}"),
        )),
    }
}

impl From<wit_types::EntryKind> for cache::EntryKindCache {
    fn from(kind: wit_types::EntryKind) -> Self {
        match kind {
            wit_types::EntryKind::Directory => Self::Directory,
            wit_types::EntryKind::File => Self::File,
        }
    }
}

impl From<ErrorKind> for wit_types::ErrorKind {
    fn from(kind: ErrorKind) -> Self {
        match kind {
            ErrorKind::Network => Self::Network,
            ErrorKind::Timeout => Self::Timeout,
            ErrorKind::Denied => Self::Denied,
            ErrorKind::NotFound => Self::NotFound,
            ErrorKind::RateLimited => Self::RateLimited,
            ErrorKind::InvalidInput => Self::InvalidInput,
            ErrorKind::TooLarge => Self::TooLarge,
            ErrorKind::Internal => Self::Internal,
        }
    }
}

fn absolute_mount_path(path: &str) -> String {
    if path.is_empty() {
        "/".to_string()
    } else if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    }
}

struct HttpCalloutResponse(CalloutResponse);

impl From<HttpCalloutResponse> for wit_types::CalloutResult {
    fn from(response: HttpCalloutResponse) -> Self {
        match response.0 {
            CalloutResponse::HttpResponse {
                status,
                headers,
                body,
            } => wit_types::CalloutResult::HttpResponse(wit_types::HttpResponse {
                status,
                headers: headers
                    .into_iter()
                    .map(|(name, value)| wit_types::Header { name, value })
                    .collect(),
                body,
            }),
            CalloutResponse::Error {
                kind,
                message,
                retryable,
            } => wit_types::CalloutResult::CalloutError(wit_types::CalloutError {
                kind: kind.into(),
                message,
                retryable,
            }),
            _ => unexpected("unexpected non-http response in http callout"),
        }
    }
}

struct GitOpenResponse(CalloutResponse);

impl From<GitOpenResponse> for wit_types::CalloutResult {
    fn from(response: GitOpenResponse) -> Self {
        match response.0 {
            CalloutResponse::GitRepoOpened(id) => {
                wit_types::CalloutResult::GitRepoOpened(wit_types::GitRepoInfo {
                    repo: id,
                    tree: id,
                })
            },
            CalloutResponse::Error {
                kind,
                message,
                retryable,
            } => wit_types::CalloutResult::CalloutError(wit_types::CalloutError {
                kind: kind.into(),
                message,
                retryable,
            }),
            _ => unexpected("unexpected response in git callout"),
        }
    }
}

struct BlobFetchResponse(CalloutResponse);

impl From<BlobFetchResponse> for wit_types::CalloutResult {
    fn from(response: BlobFetchResponse) -> Self {
        match response.0 {
            CalloutResponse::BlobFetched(record) => {
                wit_types::CalloutResult::BlobFetched(wit_types::BlobFetched {
                    blob: record.id,
                    size: record.size,
                    content_type: record.content_type,
                    etag: record.etag,
                    status: record.status,
                    response_headers: record
                        .response_headers
                        .into_iter()
                        .map(|(name, value)| wit_types::Header { name, value })
                        .collect(),
                })
            },
            CalloutResponse::Error {
                kind,
                message,
                retryable,
            } => wit_types::CalloutResult::CalloutError(wit_types::CalloutError {
                kind: kind.into(),
                message,
                retryable,
            }),
            _ => unexpected("unexpected response in fetch-blob callout"),
        }
    }
}

struct ArchiveOpenResponse(CalloutResponse);

impl From<ArchiveOpenResponse> for wit_types::CalloutResult {
    fn from(response: ArchiveOpenResponse) -> Self {
        match response.0 {
            CalloutResponse::ArchiveOpened(tree) => {
                wit_types::CalloutResult::ArchiveOpened(wit_types::ArchiveOpened { tree })
            },
            CalloutResponse::Error {
                kind,
                message,
                retryable,
            } => wit_types::CalloutResult::CalloutError(wit_types::CalloutError {
                kind: kind.into(),
                message,
                retryable,
            }),
            _ => unexpected("unexpected response in open-archive callout"),
        }
    }
}

struct BlobReadResponse(CalloutResponse);

impl From<BlobReadResponse> for wit_types::CalloutResult {
    fn from(response: BlobReadResponse) -> Self {
        match response.0 {
            CalloutResponse::BlobRead(bytes) => wit_types::CalloutResult::BlobRead(bytes),
            CalloutResponse::Error {
                kind,
                message,
                retryable,
            } => wit_types::CalloutResult::CalloutError(wit_types::CalloutError {
                kind: kind.into(),
                message,
                retryable,
            }),
            _ => unexpected("unexpected response in read-blob callout"),
        }
    }
}

fn unexpected(message: &str) -> wit_types::CalloutResult {
    wit_types::CalloutResult::CalloutError(wit_types::CalloutError {
        kind: wit_types::ErrorKind::Internal,
        message: message.to_string(),
        retryable: false,
    })
}
