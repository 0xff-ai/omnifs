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
use std::collections::{BTreeMap, BTreeSet};
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
    l2: Option<cache::l2::Cache>,
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
            match cache::l2::Cache::open(&db_path) {
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

    pub fn initialize(&self) -> Result<wit_types::OperationResult> {
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

    pub(super) fn push_projected_file_content(
        batch: &mut Vec<BatchRecord>,
        file_path: &str,
        file: &wit_types::FileProj,
    ) {
        let attrs_cache = cache::FileAttrsCache::from(file);
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
    }

    pub(super) fn push_projected_entry(
        batch: &mut Vec<BatchRecord>,
        path: &str,
        kind: &wit_types::EntryKind,
    ) {
        use cache::{AttrPayload, EntryMeta, LookupPayload};

        let meta = EntryMeta::from(kind);
        let lookup = LookupPayload::Positive(meta.clone());
        if let Some(payload) = lookup.serialize() {
            batch.push(BatchRecord::new(
                path,
                RecordKind::Lookup,
                None,
                CacheRecord::new(RecordKind::Lookup, payload),
            ));
        }

        let attr = AttrPayload { meta };
        if let Some(payload) = attr.serialize() {
            batch.push(BatchRecord::new(
                path,
                RecordKind::Attr,
                None,
                CacheRecord::new(RecordKind::Attr, payload),
            ));
        }
    }

    pub(super) fn apply_effects(&self, effects: &[wit_types::Effect]) -> Result<()> {
        use cache::{DirentRecord, DirentsPayload};

        let mut batch = Vec::new();
        let mut projected_dirs = BTreeSet::new();
        let mut projected_children: BTreeMap<String, BTreeMap<String, DirentRecord>> =
            BTreeMap::new();
        for effect in effects {
            match effect {
                wit_types::Effect::Project(entry) => {
                    if matches!(entry.kind, wit_types::EntryKind::Directory) {
                        projected_dirs.insert(entry.path.clone());
                    }
                    if let Some((parent, name)) = split_projected_path(&entry.path) {
                        projected_children.entry(parent).or_default().insert(
                            name.clone(),
                            DirentRecord {
                                name,
                                meta: EntryMeta::from(&entry.kind),
                            },
                        );
                    }
                    Self::push_projected_entry(&mut batch, &entry.path, &entry.kind);
                    if let wit_types::EntryKind::File(file) = &entry.kind {
                        Self::push_projected_file_content(&mut batch, &entry.path, file);
                    }
                },
                wit_types::Effect::InvalidatePath(path) => {
                    self.cache_delete_path(path);
                    self.invalidation.record_path(path.clone());
                },
                wit_types::Effect::InvalidatePrefix(prefix) => {
                    self.cache_delete_prefix(prefix);
                    self.invalidation.record_prefix(prefix.clone());
                },
                wit_types::Effect::DisownTree(handoff) => {
                    if self.resolve_tree_ref(handoff.tree).is_none() {
                        return Err(RuntimeError::ProviderError(format!(
                            "disown-tree effect for {:?} references unknown tree {}",
                            handoff.path, handoff.tree
                        )));
                    }
                },
            }
        }
        for dir in projected_dirs {
            let Some(children) = projected_children.remove(&dir) else {
                continue;
            };
            let (previously_exhaustive, mut existing_children) = self
                .cache_get(&dir, RecordKind::Dirents)
                .and_then(|record| DirentsPayload::deserialize(&record.payload))
                .map_or_else(
                    || (false, BTreeMap::new()),
                    |payload| {
                        (
                            payload.exhaustive,
                            payload
                                .entries
                                .into_iter()
                                .map(|entry| (entry.name.clone(), entry))
                                .collect::<BTreeMap<_, _>>(),
                        )
                    },
                );

            let introduced_child = children
                .keys()
                .any(|name| !existing_children.contains_key(name));
            existing_children.extend(children);
            if let Some(payload) = (DirentsPayload {
                entries: existing_children.into_values().collect(),
                exhaustive: previously_exhaustive && !introduced_child,
            })
            .serialize()
            {
                batch.push(BatchRecord::new(
                    dir,
                    RecordKind::Dirents,
                    None,
                    CacheRecord::new(RecordKind::Dirents, payload),
                ));
            }
        }
        if !batch.is_empty() {
            debug!(
                target: "omnifs_cache",
                kind = "project",
                count = batch.len(),
                "applying projection effects"
            );
            self.cache_put_batch(&batch);
        }
        Ok(())
    }

    pub async fn call_timer_tick(&self) -> Result<wit_types::OperationResult> {
        let id = self.correlations.allocate();
        let active_paths = self.activity_table.lock().active_path_sets();

        let step = {
            let mut store = self.store.lock();
            self.bindings.omnifs_provider_notify().call_on_event(
                &mut *store,
                id,
                &wit_types::ProviderEvent::TimerTick(wit_types::TimerTickContext { active_paths }),
            )?
        };

        self.drive_provider_step(id, step, None).await
    }

    /// Drive a provider call to completion.
    pub(super) async fn drive_provider_step(
        &self,
        id: u64,
        mut step: wit_types::ProviderStep,
        expected_handoff_path: Option<&str>,
    ) -> Result<wit_types::OperationResult> {
        loop {
            match step {
                wit_types::ProviderStep::Returned(ret) => {
                    validate_provider_return(&ret, expected_handoff_path)
                        .map_err(RuntimeError::ProviderError)?;
                    self.apply_effects(&ret.effects)?;
                    return Ok(ret.result);
                },
                wit_types::ProviderStep::Suspended(callouts) if callouts.is_empty() => {
                    return Err(RuntimeError::ProviderError(
                        "provider suspended with no callouts".into(),
                    ));
                },
                wit_types::ProviderStep::Suspended(callouts) => {
                    let results = self.execute_batch(&callouts).await;
                    let mut store = self.store.lock();
                    step = self.bindings.omnifs_provider_continuation().call_resume(
                        &mut *store,
                        id,
                        &results,
                    )?;
                },
            }
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

    fn resolve_response_sync(
        response: wit_types::ProviderReturn,
    ) -> Result<wit_types::OperationResult> {
        validate_provider_return(&response, None).map_err(RuntimeError::ProviderError)?;
        Ok(response.result)
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

#[cfg(test)]
fn validate_operation_result(
    result: &wit_types::OperationResult,
) -> std::result::Result<(), String> {
    let mut validator = AttrValidator::default();
    validate_operation_result_with(result, &mut validator)
}

fn validate_operation_result_with(
    result: &wit_types::OperationResult,
    validator: &mut AttrValidator,
) -> std::result::Result<(), String> {
    match result {
        wit_types::OperationResult::ListChildren(wit_types::ListChildrenResult::Entries(
            listing,
        )) => {
            for entry in &listing.entries {
                validator.entry(&entry.kind)?;
            }
        },
        wit_types::OperationResult::LookupChild(wit_types::LookupChildResult::Entry(entry)) => {
            validator.entry(&entry.target.kind)?;
            for sibling in &entry.siblings {
                validator.entry(&sibling.kind)?;
            }
        },
        wit_types::OperationResult::ReadFile(result) => {
            AttrValidator::file_attrs_metadata(&result.attrs)?;
            match &result.bytes {
                wit_types::ReadFileBytes::Inline(bytes) => {
                    let attrs = cache::FileAttrsCache::from(&result.attrs);
                    validate_complete_content(&attrs, bytes.len())
                        .map_err(|error| format!("read-file result: {error}"))?;
                    validator.add_eager_bytes(bytes.len())?;
                },
                wit_types::ReadFileBytes::Blob(_) => {},
            }
        },
        wit_types::OperationResult::OpenFile(result) => {
            AttrValidator::file_attrs_metadata(&result.attrs)?;
        },
        _ => {},
    }
    Ok(())
}

fn split_projected_path(path: &str) -> Option<(String, String)> {
    if path.is_empty() {
        return None;
    }
    match path.rsplit_once('/') {
        Some((parent, name)) if !name.is_empty() => Some((parent.to_string(), name.to_string())),
        None => Some((String::new(), path.to_string())),
        _ => None,
    }
}

fn validate_effects_with(
    effects: &[wit_types::Effect],
    validator: &mut AttrValidator,
) -> std::result::Result<(), String> {
    for effect in effects {
        match effect {
            wit_types::Effect::Project(entry) => validator
                .entry(&entry.kind)
                .map_err(|error| format!("project effect {:?}: {error}", entry.path))?,
            wit_types::Effect::InvalidatePath(_)
            | wit_types::Effect::InvalidatePrefix(_)
            | wit_types::Effect::DisownTree(_) => {},
        }
    }
    Ok(())
}

fn validate_provider_return(
    ret: &wit_types::ProviderReturn,
    expected_handoff_path: Option<&str>,
) -> std::result::Result<(), String> {
    if matches!(ret.result, wit_types::OperationResult::Error(_)) && !ret.effects.is_empty() {
        return Err("provider error returns must not carry effects".to_string());
    }
    let mut validator = AttrValidator::default();
    validate_operation_result_with(&ret.result, &mut validator)?;
    validate_effects_with(&ret.effects, &mut validator)?;

    let handoffs = ret
        .effects
        .iter()
        .filter_map(|effect| {
            if let wit_types::Effect::DisownTree(handoff) = effect {
                Some(handoff)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    let subtree = match &ret.result {
        wit_types::OperationResult::LookupChild(wit_types::LookupChildResult::Subtree(tree))
        | wit_types::OperationResult::ListChildren(wit_types::ListChildrenResult::Subtree(tree)) => {
            Some(*tree)
        },
        _ => None,
    };
    match subtree {
        Some(tree) => {
            if handoffs.len() != 1 || handoffs[0].tree != tree {
                return Err(format!(
                    "subtree result for tree {tree} requires exactly one matching disown-tree effect"
                ));
            }
            if let Some(expected_path) = expected_handoff_path {
                let expected_path = expected_path.trim_matches('/');
                if handoffs[0].path != expected_path {
                    return Err(format!(
                        "subtree result for tree {tree} requires disown-tree path {:?}, got {:?}",
                        expected_path, handoffs[0].path
                    ));
                }
            }
        },
        None if !handoffs.is_empty() => {
            return Err(format!(
                "disown-tree effects require a subtree result, got {} orphan effect(s)",
                handoffs.len()
            ));
        },
        None => {},
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
            wit_types::EntryKind::File(file) => self.file_proj(file),
        }
    }

    fn file_proj(&mut self, file: &wit_types::FileProj) -> std::result::Result<(), String> {
        let attrs = cache::FileAttrsCache::from(file);
        attrs.validate()?;
        self.add_eager_bytes(attrs.eager_byte_len())
    }

    fn file_attrs_metadata(attrs: &wit_types::FileAttrs) -> std::result::Result<(), String> {
        if let Some(token) = &attrs.version_token {
            if token.is_empty() {
                return Err("version token must not be empty".to_string());
            }
            if token.len() > cache::MAX_VERSION_TOKEN_BYTES {
                return Err(format!(
                    "version token exceeds {} bytes",
                    cache::MAX_VERSION_TOKEN_BYTES
                ));
            }
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

    fn attrs(size: wit_types::FileSize, stability: wit_types::Stability) -> wit_types::FileAttrs {
        wit_types::FileAttrs {
            size,
            stability,
            version_token: None,
        }
    }

    fn file_proj(
        size: wit_types::FileSize,
        bytes: wit_types::ProjBytes,
        stability: wit_types::Stability,
    ) -> wit_types::FileProj {
        wit_types::FileProj {
            attrs: attrs(size, stability),
            bytes,
        }
    }

    fn deferred_exact(size: u64) -> wit_types::FileProj {
        file_proj(
            wit_types::FileSize::Exact(size),
            wit_types::ProjBytes::Deferred(wit_types::ReadMode::Full),
            wit_types::Stability::Immutable,
        )
    }

    #[test]
    fn rejects_invalid_inline_projection_in_entries() {
        let result = wit_types::OperationResult::ListChildren(
            wit_types::ListChildrenResult::Entries(wit_types::DirListing {
                entries: vec![wit_types::DirEntry {
                    name: "bad".to_string(),
                    kind: wit_types::EntryKind::File(file_proj(
                        wit_types::FileSize::Unknown,
                        wit_types::ProjBytes::Inline(b"bad".to_vec()),
                        wit_types::Stability::Immutable,
                    )),
                }],
                exhaustive: true,
            }),
        );

        let error = validate_operation_result(&result).unwrap_err();
        assert!(error.contains("inline bytes require Size::Exact"));
    }

    #[test]
    fn rejects_volatile_non_ranged_attrs() {
        let result = wit_types::OperationResult::ListChildren(
            wit_types::ListChildrenResult::Entries(wit_types::DirListing {
                entries: vec![wit_types::DirEntry {
                    name: "tail".to_string(),
                    kind: wit_types::EntryKind::File(file_proj(
                        wit_types::FileSize::Unknown,
                        wit_types::ProjBytes::Deferred(wit_types::ReadMode::Full),
                        wit_types::Stability::Volatile,
                    )),
                }],
                exhaustive: true,
            }),
        );

        let error = validate_operation_result(&result).unwrap_err();
        assert!(error.contains("Stability::Volatile requires"));
    }

    #[test]
    fn rejects_bad_project_effect_size_and_aggregate_eager_cap() {
        let mut bad_size_file = deferred_exact(4);
        bad_size_file.bytes = wit_types::ProjBytes::Inline(b"toolong".to_vec());
        let bad_size = wit_types::ProviderReturn {
            result: wit_types::OperationResult::OnEvent,
            effects: vec![wit_types::Effect::Project(wit_types::ProjEntry {
                path: "bad".to_string(),
                kind: wit_types::EntryKind::File(bad_size_file),
            })],
        };
        let error = validate_provider_return(&bad_size, None).unwrap_err();
        assert!(error.contains("declares size 4"));

        let too_large = wit_types::ProviderReturn {
            result: wit_types::OperationResult::OnEvent,
            effects: (0..9)
                .map(|index| {
                    let bytes = vec![0; cache::MAX_PROJECTED_BYTES];
                    wit_types::Effect::Project(wit_types::ProjEntry {
                        path: format!("large-{index}"),
                        kind: wit_types::EntryKind::File(wit_types::FileProj {
                            attrs: attrs(
                                wit_types::FileSize::Exact(bytes.len() as u64),
                                wit_types::Stability::Immutable,
                            ),
                            bytes: wit_types::ProjBytes::Inline(bytes),
                        }),
                    })
                })
                .collect(),
        };
        let error = validate_provider_return(&too_large, None).unwrap_err();
        assert!(error.contains("aggregate eager byte limit"));
    }

    #[test]
    fn rejects_read_content_that_violates_declared_size() {
        let result = wit_types::OperationResult::ReadFile(wit_types::ReadFileResult {
            attrs: attrs(
                wit_types::FileSize::NonZero,
                wit_types::Stability::Immutable,
            ),
            bytes: wit_types::ReadFileBytes::Inline(Vec::new()),
        });

        let error = validate_operation_result(&result).unwrap_err();
        assert!(error.contains("read-file result"));
        assert!(error.contains("Size::NonZero"));
    }

    #[test]
    fn rejects_empty_version_tokens() {
        let mut file = deferred_exact(1);
        file.attrs.version_token = Some(String::new());
        let result = wit_types::OperationResult::ListChildren(
            wit_types::ListChildrenResult::Entries(wit_types::DirListing {
                entries: vec![wit_types::DirEntry {
                    name: "versioned".to_string(),
                    kind: wit_types::EntryKind::File(file),
                }],
                exhaustive: true,
            }),
        );

        let error = validate_operation_result(&result).unwrap_err();
        assert!(error.contains("version token must not be empty"));
    }

    #[test]
    fn subtree_results_require_matching_disown_effect() {
        let missing = wit_types::ProviderReturn {
            result: wit_types::OperationResult::LookupChild(wit_types::LookupChildResult::Subtree(
                7,
            )),
            effects: Vec::new(),
        };
        let error = validate_provider_return(&missing, Some("checkout")).unwrap_err();
        assert!(error.contains("requires exactly one matching disown-tree effect"));

        let valid = wit_types::ProviderReturn {
            result: wit_types::OperationResult::LookupChild(wit_types::LookupChildResult::Subtree(
                7,
            )),
            effects: vec![wit_types::Effect::DisownTree(wit_types::TreeHandoff {
                path: "checkout".to_string(),
                tree: 7,
            })],
        };
        validate_provider_return(&valid, Some("checkout")).unwrap();

        let error = validate_provider_return(&valid, Some("other")).unwrap_err();
        assert!(error.contains("requires disown-tree path"));

        let orphan = wit_types::ProviderReturn {
            result: wit_types::OperationResult::OnEvent,
            effects: vec![wit_types::Effect::DisownTree(wit_types::TreeHandoff {
                path: "checkout".to_string(),
                tree: 7,
            })],
        };
        let error = validate_provider_return(&orphan, None).unwrap_err();
        assert!(error.contains("require a subtree result"));
    }

    #[test]
    fn error_returns_reject_effects() {
        let ret = wit_types::ProviderReturn {
            result: wit_types::OperationResult::Error(wit_types::ProviderError {
                kind: wit_types::ErrorKind::Internal,
                message: "failed".to_string(),
                retryable: false,
            }),
            effects: vec![wit_types::Effect::InvalidatePath("x".to_string())],
        };

        let error = validate_provider_return(&ret, None).unwrap_err();
        assert!(error.contains("error returns must not carry effects"));
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

impl From<&wit_types::FileProj> for cache::FileAttrsCache {
    fn from(file: &wit_types::FileProj) -> Self {
        Self {
            size: SizeCache::from(&file.attrs.size),
            bytes: cache::BytesCache::from(&file.bytes),
            stability: cache::StabilityCache::from(file.attrs.stability),
            version_token: file.attrs.version_token.clone(),
        }
    }
}

impl From<&wit_types::FileAttrs> for cache::FileAttrsCache {
    fn from(attrs: &wit_types::FileAttrs) -> Self {
        Self {
            size: SizeCache::from(&attrs.size),
            bytes: cache::BytesCache::Deferred(cache::ReadModeCache::Full),
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

impl From<&wit_types::ProjBytes> for cache::BytesCache {
    fn from(bytes: &wit_types::ProjBytes) -> Self {
        match bytes {
            wit_types::ProjBytes::Inline(bytes) => Self::Inline(bytes.clone()),
            wit_types::ProjBytes::Deferred(mode) => {
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
            wit_types::EntryKind::File(file) => Self::file(cache::FileAttrsCache::from(file)),
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
