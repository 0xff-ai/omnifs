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
use crate::cache::{BatchRecord, CacheRecord, EntryMeta, FilePayload, Key, RecordKind, SizeCache};
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
use fuser::Notifier;
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{debug, error, info, trace, warn};
use wasmtime::component::{Component, HasData, Linker, ResourceTable};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

const ACTIVITY_TTL: std::time::Duration = std::time::Duration::from_secs(600);

/// FUSE notifier handle (only available on Linux with FUSE support).
pub type NotifierHandle = Arc<Mutex<Option<Notifier>>>;

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
            wit_types::LogLevel::Trace => trace!("{}", entry.message),
            wit_types::LogLevel::Debug => debug!("{}", entry.message),
            wit_types::LogLevel::Info => info!("{}", entry.message),
            wit_types::LogLevel::Warn => warn!("{}", entry.message),
            wit_types::LogLevel::Error => error!("{}", entry.message),
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
            warn!(
                dir = %blob_cache_dir.display(),
                error = %e,
                "failed to create blob cache dir; fetch-blob will fail until resolved"
            );
        }
        if let Err(e) = std::fs::create_dir_all(&archive_root) {
            warn!(
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
                    warn!(mount = mount_name, error = %e, "failed to open L2 browse cache");
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

    pub fn cache_get_with_aux(
        &self,
        path: &str,
        kind: RecordKind,
        aux: Option<&str>,
    ) -> Option<CacheRecord> {
        self.l2
            .as_ref()?
            .get(&Key::with_aux(path, kind, aux))
            .ok()
            .flatten()
    }

    pub fn cache_put(&self, path: &str, kind: RecordKind, record: &CacheRecord) {
        if let Some(ref l2) = self.l2
            && let Err(e) = l2.put(&Key::new(path, kind), record)
        {
            debug!(path, error = %e, "L2 cache put failed");
        }
    }

    pub fn cache_put_with_aux(
        &self,
        path: &str,
        kind: RecordKind,
        aux: Option<&str>,
        record: &CacheRecord,
    ) {
        if let Some(ref l2) = self.l2
            && let Err(e) = l2.put(&Key::with_aux(path, kind, aux), record)
        {
            debug!(path, error = %e, "L2 cache put failed");
        }
    }

    pub fn cache_put_batch(&self, records: &[BatchRecord]) {
        if let Some(ref l2) = self.l2
            && let Err(e) = l2.put_batch(records)
        {
            debug!(error = %e, "L2 cache batch put failed");
        }
    }

    #[doc(hidden)]
    pub fn __active_path_sets(&self) -> Vec<wit_types::ActivePathSet> {
        self.activity_table.lock().active_path_sets()
    }

    fn push_projected_file(
        batch: &mut Vec<BatchRecord>,
        file_path: &str,
        attrs: &wit_types::FileAttrs,
    ) {
        use cache::{AttrPayload, EntryMeta, LookupPayload};

        let attrs_cache = cache::FileAttrsCache::from(attrs);
        let meta = EntryMeta::file(attrs_cache.clone());

        if let Some(content) = attrs_cache.inline_bytes()
            && let Some(aux) = attrs_cache.durable_cache_aux()
        {
            let payload = FilePayload::new(attrs_cache.version_token.clone(), content.to_vec());
            if let Some(payload) = payload.serialize() {
                batch.push(BatchRecord::new(
                    file_path,
                    RecordKind::File,
                    aux,
                    CacheRecord::new(RecordKind::File, payload),
                ));
            }
        }

        let pf_lookup = LookupPayload::Positive(meta.clone());
        if let Some(payload) = pf_lookup.serialize() {
            batch.push(BatchRecord::new(
                file_path,
                RecordKind::Lookup,
                None,
                CacheRecord::new(RecordKind::Lookup, payload),
            ));
        }

        let pf_attr = AttrPayload { meta };
        if let Some(payload) = pf_attr.serialize() {
            batch.push(BatchRecord::new(
                file_path,
                RecordKind::Attr,
                None,
                CacheRecord::new(RecordKind::Attr, payload),
            ));
        }
    }

    fn push_preloaded_entry(batch: &mut Vec<BatchRecord>, entry: &wit_types::PreloadedEntry) {
        use cache::{AttrPayload, EntryMeta, LookupPayload};

        let meta = EntryMeta::from(&entry.kind);
        let lookup = LookupPayload::Positive(meta.clone());
        if let Some(payload) = lookup.serialize() {
            batch.push(BatchRecord::new(
                entry.path.clone(),
                RecordKind::Lookup,
                None,
                CacheRecord::new(RecordKind::Lookup, payload),
            ));
        }

        let attr = AttrPayload { meta };
        if let Some(payload) = attr.serialize() {
            batch.push(BatchRecord::new(
                entry.path.clone(),
                RecordKind::Attr,
                None,
                CacheRecord::new(RecordKind::Attr, payload),
            ));
        }
    }

    fn push_preloaded_file_content(
        batch: &mut Vec<BatchRecord>,
        file_path: &str,
        attrs: &wit_types::FileAttrs,
        content: &[u8],
    ) {
        let attrs_cache = cache::FileAttrsCache::from(attrs);
        let Some(aux) = attrs_cache.durable_cache_aux() else {
            return;
        };
        let payload = FilePayload::new(attrs_cache.version_token.clone(), content.to_vec());
        if let Some(payload) = payload.serialize() {
            batch.push(BatchRecord::new(
                file_path,
                RecordKind::File,
                aux,
                CacheRecord::new(RecordKind::File, payload),
            ));
        }
    }

    pub(super) fn apply_preloads(&self, items: &[wit_types::PreloadItem]) {
        use cache::{DirentRecord, DirentsPayload, EntryKindCache, EntryMeta};

        if items.is_empty() {
            return;
        }
        let mut batch = Vec::new();
        let mut child_records: BTreeMap<String, BTreeMap<String, DirentRecord>> = BTreeMap::new();
        let mut preloaded_dirs = Vec::new();
        for item in items {
            match item {
                wit_types::PreloadItem::File(file) => {
                    Self::push_preloaded_file_content(
                        &mut batch,
                        &file.path,
                        &file.attrs,
                        &file.content,
                    );
                    Self::record_preload_child(
                        &mut child_records,
                        &file.path,
                        EntryMeta::file(cache::FileAttrsCache::from(&file.attrs)),
                    );
                },
                wit_types::PreloadItem::Entry(entry) => {
                    Self::push_preloaded_entry(&mut batch, entry);
                    let meta = EntryMeta::from(&entry.kind);
                    if matches!(meta.kind, EntryKindCache::Directory) {
                        preloaded_dirs.push(entry.path.clone());
                    }
                    Self::record_preload_child(&mut child_records, &entry.path, meta);
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
                batch.push(BatchRecord::new(
                    path,
                    RecordKind::Dirents,
                    None,
                    CacheRecord::new(RecordKind::Dirents, payload),
                ));
            }
        }

        if !batch.is_empty() {
            debug!(
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
        meta: EntryMeta,
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
                meta,
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
                    validate_terminal_attrs(&terminal).map_err(RuntimeError::ProviderError)?;
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
            .and_then(|terminal| {
                validate_terminal_attrs(&terminal).map_err(RuntimeError::ProviderError)?;
                Ok(terminal)
            })
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
                callout_response_to_wit(resp)
            },
            wit_types::Callout::GitOpenRepo(req) => {
                git_response_to_wit(self.git.open_repo(&req.cache_key, &req.clone_url))
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
                blob_response_to_wit(resp)
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
                archive_response_to_wit(resp)
            },
            wit_types::Callout::ReadBlob(req) => {
                blob_read_to_wit(self.blob.read_blob(req.blob, req.offset, req.len))
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

fn validate_terminal_attrs(terminal: &wit_types::OpResult) -> std::result::Result<(), String> {
    let mut validator = AttrValidator::default();
    match terminal {
        wit_types::OpResult::List(wit_types::ListResult::Entries(listing)) => {
            for entry in &listing.entries {
                validator.entry(&entry.kind)?;
            }
            validator.preloads(&listing.preload)?;
        },
        wit_types::OpResult::Lookup(wit_types::LookupResult::Entry(entry)) => {
            validator.entry(&entry.target.kind)?;
            for sibling in &entry.siblings {
                validator.entry(&sibling.kind)?;
            }
            for file in &entry.sibling_files {
                validator.projected_file(file)?;
            }
            validator.preloads(&entry.preload)?;
        },
        wit_types::OpResult::Read(result) => match result {
            wit_types::FileContentResult::Inline(inline) => {
                validator.file_attrs(&inline.attrs)?;
                let attrs = cache::FileAttrsCache::from(&inline.attrs);
                validate_complete_content(&attrs, inline.content.len())
                    .map_err(|error| format!("read result: {error}"))?;
                for file in &inline.sibling_files {
                    validator.projected_file(file)?;
                }
            },
            wit_types::FileContentResult::Blob(blob) => {
                validator.file_attrs(&blob.attrs)?;
                for file in &blob.sibling_files {
                    validator.projected_file(file)?;
                }
            },
        },
        wit_types::OpResult::OpenFile(result) => {
            validator.file_attrs(&result.attrs)?;
            if !matches!(
                &result.attrs.bytes,
                wit_types::FileBytes::Deferred(wit_types::ReadMode::Ranged)
            ) {
                return Err("open-file requires Bytes::Deferred { read: ReadMode::Ranged }".into());
            }
        },
        _ => {},
    }
    Ok(())
}

#[derive(Default)]
struct AttrValidator {
    eager_bytes: usize,
}

impl AttrValidator {
    fn entry(&mut self, kind: &wit_types::EntryKind) -> std::result::Result<(), String> {
        match kind {
            wit_types::EntryKind::Directory => Ok(()),
            wit_types::EntryKind::File(attrs) => self.file_attrs(attrs),
        }
    }

    fn projected_file(
        &mut self,
        file: &wit_types::ProjectedFile,
    ) -> std::result::Result<(), String> {
        self.file_attrs(&file.attrs)
            .map_err(|error| format!("projected file {:?}: {error}", file.name))
    }

    fn file_attrs(&mut self, attrs: &wit_types::FileAttrs) -> std::result::Result<(), String> {
        let attrs = cache::FileAttrsCache::from(attrs);
        attrs.validate()?;
        self.add_eager_bytes(attrs.eager_byte_len())
    }

    fn preloads(&mut self, items: &[wit_types::PreloadItem]) -> std::result::Result<(), String> {
        for item in items {
            let wit_types::PreloadItem::Entry(entry) = item else {
                continue;
            };
            self.entry(&entry.kind)
                .map_err(|error| format!("preloaded entry {:?}: {error}", entry.path))?;
        }

        for item in items {
            let wit_types::PreloadItem::File(file) = item else {
                continue;
            };
            let attrs = cache::FileAttrsCache::from(&file.attrs);
            attrs
                .validate()
                .map_err(|error| format!("preloaded file {:?}: {error}", file.path))?;
            validate_complete_content(&attrs, file.content.len())
                .map_err(|error| format!("preloaded file {:?}: {error}", file.path))?;
            if matches!(attrs.stability, cache::StabilityCache::Volatile) {
                return Err(format!(
                    "preloaded file {:?}: Stability::Volatile cannot carry whole-file preload bytes",
                    file.path
                ));
            }
            self.add_eager_bytes(file.content.len())?;
        }
        Ok(())
    }

    fn add_eager_bytes(&mut self, bytes: usize) -> std::result::Result<(), String> {
        self.eager_bytes = self
            .eager_bytes
            .checked_add(bytes)
            .ok_or_else(|| "aggregate eager byte count overflowed".to_string())?;
        if self.eager_bytes > cache::MAX_EAGER_RESPONSE_BYTES {
            return Err(format!(
                "terminal response exceeds aggregate eager byte limit of {} bytes",
                cache::MAX_EAGER_RESPONSE_BYTES
            ));
        }
        Ok(())
    }
}

fn validate_complete_content(
    attrs: &cache::FileAttrsCache,
    content_len: usize,
) -> std::result::Result<(), String> {
    attrs.validate_complete_content(content_len)
}

#[cfg(test)]
mod attr_contract_tests {
    use super::*;

    fn attrs(
        size: wit_types::FileSize,
        bytes: wit_types::FileBytes,
        stability: wit_types::Stability,
    ) -> wit_types::FileAttrs {
        wit_types::FileAttrs {
            size,
            bytes,
            stability,
            version_token: None,
        }
    }

    fn deferred_exact(size: u64) -> wit_types::FileAttrs {
        attrs(
            wit_types::FileSize::Exact(size),
            wit_types::FileBytes::Deferred(wit_types::ReadMode::Full),
            wit_types::Stability::Immutable,
        )
    }

    #[test]
    fn rejects_invalid_inline_attrs_in_entries() {
        let terminal =
            wit_types::OpResult::List(wit_types::ListResult::Entries(wit_types::DirListing {
                entries: vec![wit_types::DirEntry {
                    name: "bad".to_string(),
                    kind: wit_types::EntryKind::File(attrs(
                        wit_types::FileSize::Unknown,
                        wit_types::FileBytes::Inline(b"bad".to_vec()),
                        wit_types::Stability::Immutable,
                    )),
                }],
                exhaustive: true,
                preload: Vec::new(),
            }));

        let error = validate_terminal_attrs(&terminal).unwrap_err();
        assert!(error.contains("inline bytes require Size::Exact"));
    }

    #[test]
    fn rejects_volatile_non_ranged_attrs() {
        let terminal =
            wit_types::OpResult::List(wit_types::ListResult::Entries(wit_types::DirListing {
                entries: vec![wit_types::DirEntry {
                    name: "tail".to_string(),
                    kind: wit_types::EntryKind::File(attrs(
                        wit_types::FileSize::Unknown,
                        wit_types::FileBytes::Deferred(wit_types::ReadMode::Full),
                        wit_types::Stability::Volatile,
                    )),
                }],
                exhaustive: true,
                preload: Vec::new(),
            }));

        let error = validate_terminal_attrs(&terminal).unwrap_err();
        assert!(error.contains("Stability::Volatile requires"));
    }

    #[test]
    fn rejects_bad_preload_size_and_aggregate_eager_cap() {
        let bad_size =
            wit_types::OpResult::List(wit_types::ListResult::Entries(wit_types::DirListing {
                entries: Vec::new(),
                exhaustive: true,
                preload: vec![wit_types::PreloadItem::File(wit_types::PreloadedFile {
                    path: "bad".to_string(),
                    attrs: deferred_exact(4),
                    content: b"toolong".to_vec(),
                })],
            }));
        let error = validate_terminal_attrs(&bad_size).unwrap_err();
        assert!(error.contains("declares exact size 4"));

        let too_large =
            wit_types::OpResult::List(wit_types::ListResult::Entries(wit_types::DirListing {
                entries: Vec::new(),
                exhaustive: true,
                preload: vec![wit_types::PreloadItem::File(wit_types::PreloadedFile {
                    path: "large".to_string(),
                    attrs: deferred_exact((cache::MAX_EAGER_RESPONSE_BYTES + 1) as u64),
                    content: vec![0; cache::MAX_EAGER_RESPONSE_BYTES + 1],
                })],
            }));
        let error = validate_terminal_attrs(&too_large).unwrap_err();
        assert!(error.contains("aggregate eager byte limit"));
    }

    #[test]
    fn rejects_read_content_that_violates_declared_size() {
        let terminal = wit_types::OpResult::Read(wit_types::FileContentResult::Inline(
            wit_types::InlineFileContent {
                attrs: attrs(
                    wit_types::FileSize::NonZero,
                    wit_types::FileBytes::Deferred(wit_types::ReadMode::Full),
                    wit_types::Stability::Immutable,
                ),
                content: Vec::new(),
                sibling_files: Vec::new(),
            },
        ));

        let error = validate_terminal_attrs(&terminal).unwrap_err();
        assert!(error.contains("read result"));
        assert!(error.contains("Size::NonZero"));
    }

    #[test]
    fn rejects_empty_version_tokens() {
        let mut file_attrs = deferred_exact(1);
        file_attrs.version_token = Some(String::new());
        let terminal =
            wit_types::OpResult::List(wit_types::ListResult::Entries(wit_types::DirListing {
                entries: vec![wit_types::DirEntry {
                    name: "versioned".to_string(),
                    kind: wit_types::EntryKind::File(file_attrs),
                }],
                exhaustive: true,
                preload: Vec::new(),
            }));

        let error = validate_terminal_attrs(&terminal).unwrap_err();
        assert!(error.contains("version token must not be empty"));
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

impl From<&wit_types::FileAttrs> for cache::FileAttrsCache {
    fn from(attrs: &wit_types::FileAttrs) -> Self {
        Self {
            size: SizeCache::from(&attrs.size),
            bytes: cache::BytesCache::from(&attrs.bytes),
            stability: cache::StabilityCache::from(attrs.stability),
            version_token: attrs.version_token.clone(),
        }
    }
}

impl From<&wit_types::FileSize> for SizeCache {
    fn from(size: &wit_types::FileSize) -> Self {
        match size {
            wit_types::FileSize::Exact(size) => Self::Exact(*size),
            wit_types::FileSize::NonZero => Self::NonZero,
            wit_types::FileSize::Unknown => Self::Unknown,
        }
    }
}

impl From<&wit_types::FileBytes> for cache::BytesCache {
    fn from(bytes: &wit_types::FileBytes) -> Self {
        match bytes {
            wit_types::FileBytes::Inline(bytes) => Self::Inline(bytes.clone()),
            wit_types::FileBytes::Deferred(mode) => {
                Self::Deferred(cache::ReadModeCache::from(*mode))
            },
        }
    }
}

impl From<wit_types::ReadMode> for cache::ReadModeCache {
    fn from(mode: wit_types::ReadMode) -> Self {
        match mode {
            wit_types::ReadMode::Full => Self::Full,
            wit_types::ReadMode::Ranged => Self::Ranged,
        }
    }
}

impl From<wit_types::Stability> for cache::StabilityCache {
    fn from(stability: wit_types::Stability) -> Self {
        match stability {
            wit_types::Stability::Immutable => Self::Immutable,
            wit_types::Stability::Mutable => Self::Mutable,
            wit_types::Stability::Volatile => Self::Volatile,
        }
    }
}

impl From<&wit_types::EntryKind> for EntryMeta {
    fn from(kind: &wit_types::EntryKind) -> Self {
        match kind {
            wit_types::EntryKind::Directory => Self::directory(),
            wit_types::EntryKind::File(attrs) => Self::file(cache::FileAttrsCache::from(attrs)),
        }
    }
}

impl From<&wit_types::EntryKind> for cache::EntryKindCache {
    fn from(kind: &wit_types::EntryKind) -> Self {
        match kind {
            wit_types::EntryKind::Directory => Self::Directory,
            wit_types::EntryKind::File(_) => Self::File,
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

fn callout_response_to_wit(resp: CalloutResponse) -> wit_types::CalloutResult {
    match resp {
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

fn git_response_to_wit(resp: CalloutResponse) -> wit_types::CalloutResult {
    match resp {
        CalloutResponse::GitRepoOpened(id) => {
            wit_types::CalloutResult::GitRepoOpened(wit_types::GitRepoInfo { repo: id, tree: id })
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

fn blob_response_to_wit(resp: CalloutResponse) -> wit_types::CalloutResult {
    match resp {
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

fn archive_response_to_wit(resp: CalloutResponse) -> wit_types::CalloutResult {
    match resp {
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

fn blob_read_to_wit(resp: CalloutResponse) -> wit_types::CalloutResult {
    match resp {
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

fn unexpected(message: &str) -> wit_types::CalloutResult {
    wit_types::CalloutResult::CalloutError(wit_types::CalloutError {
        kind: wit_types::ErrorKind::Internal,
        message: message.to_string(),
        retryable: false,
    })
}
