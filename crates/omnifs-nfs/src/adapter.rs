use crate::export::{
    Attr, DirEntry, DirListing, NodeKind, OpenRead, OpenResult, OpenSeed, OpenTable,
    ReadOnlyExport, StateId, StateIdOther, Status, StatusResult,
};
use crate::frontend;
use crate::frontend::{LookupCacheHit, ProviderFsError};
use crate::protocol::consts::{
    EXPORT_ROOT_ID, MAX_NFS_READ_BYTES, NFS_EXPORT_NAME, OPEN_MATERIALIZE_MAX_BYTES, ROOT_ID,
};
use dashmap::DashMap;
use omnifs_cache::{Record as CacheRecord, RecordKind};
use omnifs_core::path::{Path as ProtocolPath, Segment};
use omnifs_core::view as view_types;
use omnifs_core::view::{self as cache, EntryMeta, FileAttrsCache, FilePayload};
use omnifs_host::path_key::PathKey;
use omnifs_host::registry::ProviderRegistry;
use omnifs_host::wit_protocol;
use omnifs_host::{Error as RuntimeError, LookupEntry, LookupOutcome, Runtime};
use omnifs_wit::provider::types::{self as wit_types, ListChildrenResult, ProviderError};
use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};
use tokio::runtime::{Handle, RuntimeFlavor};

const ENTRY_IDLE_TTL: Duration = Duration::from_secs(300);
const ENTRY_SWEEP_INTERVAL: u64 = 1024;

#[derive(Debug, Clone)]
struct ObjectRecord {
    scope: u64,
    mount_name: String,
    path: ProtocolPath,
    parent: u64,
    kind: NodeKind,
    size: u64,
    size_exact: bool,
    attrs: Option<FileAttrsCache>,
    backing_path: Option<PathBuf>,
    last_seen: Instant,
}

struct EntrySeed<'a> {
    scope: u64,
    mount_name: &'a str,
    path: &'a ProtocolPath,
    parent: u64,
    kind: NodeKind,
    size: u64,
    size_exact: bool,
    attrs: Option<FileAttrsCache>,
    backing_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct RangedOpen {
    mount_name: String,
    path: ProtocolPath,
    provider_handle: u64,
    attrs: FileAttrsCache,
}

#[derive(Debug, Clone)]
struct BackingOpen {
    id: u64,
    mount_name: String,
    path: ProtocolPath,
    backing_path: PathBuf,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ObjectKey {
    scope: u64,
    mount: String,
    path: ProtocolPath,
}

impl ObjectKey {
    fn new(scope: u64, mount: impl Into<String>, path: &ProtocolPath) -> Self {
        Self {
            scope,
            mount: mount.into(),
            path: path.clone(),
        }
    }
}

pub struct Export {
    rt: Handle,
    registry: Arc<dyn RegistryView>,
    entries: DashMap<u64, ObjectRecord>,
    path_to_inode: DashMap<ObjectKey, u64>,
    negative_lookups: DashMap<PathKey, ()>,
    next_ino: AtomicU64,
    sweep_counter: AtomicU64,
    root_mount: Option<String>,
    opens: OpenTable,
    ranged_opens: DashMap<StateIdOther, RangedOpen>,
    backing_opens: DashMap<StateIdOther, BackingOpen>,
}

trait RegistryView: Send + Sync {
    fn get(&self, mount: &str) -> Option<Arc<Runtime>>;
    fn mounts(&self) -> Vec<String>;
    fn root_mount_name(&self) -> Option<String>;
}

impl RegistryView for ProviderRegistry {
    fn get(&self, mount: &str) -> Option<Arc<Runtime>> {
        ProviderRegistry::get(self, mount)
    }

    fn mounts(&self) -> Vec<String> {
        ProviderRegistry::mounts(self)
    }

    fn root_mount_name(&self) -> Option<String> {
        ProviderRegistry::root_mount_name(self)
    }
}

impl Export {
    pub fn new(rt: Handle, registry: Arc<ProviderRegistry>) -> Self {
        Self::with_registry(rt, registry)
    }

    fn with_registry(rt: Handle, registry: Arc<dyn RegistryView>) -> Self {
        assert!(
            !matches!(rt.runtime_flavor(), RuntimeFlavor::CurrentThread),
            "NFS adapter requires a multi-thread Tokio runtime because sync NFS workers call Handle::block_on"
        );
        let root_mount = registry.root_mount_name();
        let entries = DashMap::new();
        let path_to_inode = DashMap::new();
        let now = Instant::now();
        let root_entry = ObjectRecord {
            scope: ROOT_ID,
            mount_name: root_mount.clone().unwrap_or_default(),
            path: ProtocolPath::root(),
            parent: ROOT_ID,
            kind: NodeKind::Directory,
            size: 0,
            size_exact: true,
            attrs: None,
            backing_path: None,
            last_seen: now,
        };
        if let Some(mount) = &root_mount {
            path_to_inode.insert(
                ObjectKey::new(ROOT_ID, mount, &ProtocolPath::root()),
                ROOT_ID,
            );
        }
        let export_root_entry = ObjectRecord {
            scope: EXPORT_ROOT_ID,
            mount_name: root_mount.clone().unwrap_or_default(),
            path: ProtocolPath::root(),
            parent: ROOT_ID,
            kind: NodeKind::Directory,
            size: 0,
            size_exact: true,
            attrs: None,
            backing_path: None,
            last_seen: now,
        };
        if let Some(mount) = &root_mount {
            path_to_inode.insert(
                ObjectKey::new(EXPORT_ROOT_ID, mount, &ProtocolPath::root()),
                EXPORT_ROOT_ID,
            );
        }
        entries.insert(ROOT_ID, root_entry);
        entries.insert(EXPORT_ROOT_ID, export_root_entry);
        Self {
            rt,
            registry,
            entries,
            path_to_inode,
            negative_lookups: DashMap::new(),
            next_ino: AtomicU64::new(EXPORT_ROOT_ID + 1),
            sweep_counter: AtomicU64::new(0),
            root_mount,
            opens: OpenTable::new(),
            ranged_opens: DashMap::new(),
            backing_opens: DashMap::new(),
        }
    }

    fn runtime_for_mount(&self, mount: &str) -> Option<Arc<Runtime>> {
        self.registry.get(mount)
    }

    fn touch_entry(&self, id: u64) {
        if let Some(mut entry) = self.entries.get_mut(&id) {
            entry.last_seen = Instant::now();
        }
    }

    fn remove_object(&self, id: u64) -> Option<ObjectRecord> {
        let (_, entry) = self.entries.remove(&id)?;
        self.path_to_inode
            .remove(&ObjectKey::new(entry.scope, &entry.mount_name, &entry.path));
        Some(entry)
    }

    fn maybe_sweep_entries(&self) {
        let count = self.sweep_counter.fetch_add(1, Ordering::Relaxed) + 1;
        if count.is_multiple_of(ENTRY_SWEEP_INTERVAL) {
            self.sweep_idle_entries();
        }
    }

    fn sweep_idle_entries(&self) {
        let now = Instant::now();
        let open_inodes = self.opens.active_inodes();
        let parent_inodes = self
            .entries
            .iter()
            .map(|entry| entry.value().parent)
            .collect::<HashSet<_>>();
        let stale = self
            .entries
            .iter()
            .filter_map(|entry| {
                let id = *entry.key();
                let entry = entry.value();
                (id != ROOT_ID
                    && id != EXPORT_ROOT_ID
                    && !entry.path.is_root()
                    && !open_inodes.contains(&id)
                    && !parent_inodes.contains(&id)
                    && now.duration_since(entry.last_seen) >= ENTRY_IDLE_TTL)
                    .then_some(id)
            })
            .collect::<Vec<_>>();

        for id in stale {
            self.remove_object(id);
        }
    }

    fn drain_invalidations_for_mount(&self, mount_name: &str) {
        let Some(runtime) = self.runtime_for_mount(mount_name) else {
            return;
        };
        let prefixes = runtime.drain_invalidated_prefixes();
        let paths = runtime.drain_invalidated_paths();
        if prefixes.is_empty() && paths.is_empty() {
            return;
        }
        let invalidations = frontend::InvalidationSet::from_raw(paths, prefixes);

        let stale_negative_lookups = self
            .negative_lookups
            .iter()
            .filter_map(|entry| {
                let key = entry.key();
                let path = ProtocolPath::parse(&key.path).ok()?;
                (key.mount == mount_name && invalidations.matches(&path)).then(|| key.clone())
            })
            .collect::<Vec<_>>();
        for key in stale_negative_lookups {
            self.negative_lookups.remove(&key);
        }

        let stale_paths = self
            .path_to_inode
            .iter()
            .filter_map(|entry| {
                let key = entry.key();
                (key.mount == mount_name && !key.path.is_root() && invalidations.matches(&key.path))
                    .then(|| key.clone())
            })
            .collect::<Vec<_>>();
        let mut stale_inodes = Vec::with_capacity(stale_paths.len());
        for key in &stale_paths {
            let Some(id) = self.path_to_inode.get(key).map(|entry| *entry.value()) else {
                continue;
            };
            if self.remove_object(id).is_some() {
                stale_inodes.push(id);
            }
        }
        self.opens.remove_inodes(&stale_inodes);

        let stale_opens = self
            .ranged_opens
            .iter()
            .filter_map(|entry| {
                let ranged = entry.value();
                (ranged.mount_name == mount_name && invalidations.matches(&ranged.path))
                    .then(|| *entry.key())
            })
            .collect::<Vec<_>>();
        for stateid in stale_opens {
            if let Some((_, ranged)) = self.ranged_opens.remove(&stateid) {
                self.close_ranged_provider_handle(&ranged);
            }
        }

        let stale_backing_opens = self
            .backing_opens
            .iter()
            .filter_map(|entry| {
                let backing = entry.value();
                (backing.mount_name == mount_name && invalidations.matches(&backing.path))
                    .then(|| *entry.key())
            })
            .collect::<Vec<_>>();
        for stateid in stale_backing_opens {
            self.backing_opens.remove(&stateid);
        }
    }

    fn alloc_ino(&self) -> u64 {
        self.next_ino.fetch_add(1, Ordering::Relaxed)
    }

    fn get_or_alloc(&self, seed: EntrySeed<'_>) -> u64 {
        let EntrySeed {
            scope,
            mount_name,
            path,
            parent,
            kind,
            size,
            size_exact,
            attrs,
            backing_path,
        } = seed;
        let key = ObjectKey::new(scope, mount_name, path);
        let attrs_for_update = attrs.clone();
        let now = Instant::now();
        let id = *self
            .path_to_inode
            .entry(key)
            .and_modify(|existing| {
                if let Some(mut entry) = self.entries.get_mut(existing) {
                    entry.last_seen = now;
                    entry.parent = parent;
                    entry.kind = kind;
                    if size_exact || !entry.size_exact {
                        entry.size = size;
                        entry.size_exact = size_exact;
                    }
                    if let Some(incoming_attrs) = attrs_for_update.clone()
                        && let Some(merged_attrs) =
                            frontend::merge_file_attrs(entry.attrs.as_ref(), Some(incoming_attrs))
                    {
                        entry.size = merged_attrs.st_size();
                        entry.size_exact =
                            matches!(merged_attrs.size, view_types::FileSize::Exact(_));
                        entry.attrs = Some(merged_attrs);
                    }
                    if backing_path.is_some() {
                        entry.backing_path.clone_from(&backing_path);
                        entry.size_exact = true;
                        entry.attrs = None;
                    }
                }
            })
            .or_insert_with(|| {
                let id = self.alloc_ino();
                self.entries.insert(
                    id,
                    ObjectRecord {
                        mount_name: mount_name.to_string(),
                        path: path.clone(),
                        scope,
                        parent,
                        kind,
                        size,
                        size_exact,
                        attrs,
                        backing_path,
                        last_seen: now,
                    },
                );
                id
            });
        self.maybe_sweep_entries();
        id
    }

    fn promote_file_attrs(&self, id: u64, attrs: FileAttrsCache) {
        if matches!(attrs.stability, view_types::Stability::Live) {
            return;
        }
        if let Some(mut entry) = self.entries.get_mut(&id)
            && entry.kind == NodeKind::File
            && entry.backing_path.is_none()
        {
            entry.size = attrs.st_size();
            entry.size_exact = matches!(attrs.size, view_types::FileSize::Exact(_));
            entry.attrs = Some(attrs);
            entry.last_seen = Instant::now();
        }
    }

    fn attr_from_entry(id: u64, entry: &ObjectRecord) -> Attr {
        Attr {
            id,
            parent: entry.parent,
            kind: entry.kind,
            size: entry.size,
            mode: entry.kind.mode(),
            change: Self::entry_change(id, entry),
            mtime_sec: 0,
        }
    }

    fn attr_from_metadata(
        id: u64,
        parent: u64,
        metadata: &std::fs::Metadata,
    ) -> StatusResult<Attr> {
        let kind = Self::backing_kind(metadata)?;
        let mtime_sec = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |duration| {
                i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
            });
        Ok(Attr {
            id,
            parent,
            kind,
            size: metadata.len(),
            mode: kind.mode(),
            change: Self::metadata_change(id, metadata),
            mtime_sec,
        })
    }

    fn backing_kind(metadata: &std::fs::Metadata) -> StatusResult<NodeKind> {
        if metadata.is_dir() {
            Ok(NodeKind::Directory)
        } else if metadata.file_type().is_symlink() {
            Ok(NodeKind::Symlink)
        } else if metadata.is_file() {
            Ok(NodeKind::File)
        } else {
            Err(Status::Invalid)
        }
    }

    fn entry_change(id: u64, entry: &ObjectRecord) -> u64 {
        let mut hasher = DefaultHasher::new();
        id.hash(&mut hasher);
        entry.mount_name.hash(&mut hasher);
        entry.path.hash(&mut hasher);
        (entry.kind as u8).hash(&mut hasher);
        entry.size.hash(&mut hasher);
        entry.size_exact.hash(&mut hasher);
        if let Some(attrs) = &entry.attrs {
            attrs.version_token.hash(&mut hasher);
            std::mem::discriminant(&attrs.size).hash(&mut hasher);
            std::mem::discriminant(&attrs.bytes).hash(&mut hasher);
            std::mem::discriminant(&attrs.stability).hash(&mut hasher);
        }
        hasher.finish()
    }

    fn metadata_change(id: u64, metadata: &std::fs::Metadata) -> u64 {
        let mut hasher = DefaultHasher::new();
        id.hash(&mut hasher);
        metadata.len().hash(&mut hasher);
        if let Ok(modified) = metadata.modified()
            && let Ok(duration) = modified.duration_since(UNIX_EPOCH)
        {
            duration.as_secs().hash(&mut hasher);
            duration.subsec_nanos().hash(&mut hasher);
        }
        hasher.finish()
    }

    fn meta_kind(meta: &EntryMeta) -> NodeKind {
        match &meta.kind {
            view_types::EntryKind::Directory => NodeKind::Directory,
            view_types::EntryKind::File => NodeKind::File,
        }
    }

    fn meta_size(meta: &EntryMeta) -> (u64, bool) {
        let exact = match &meta.attrs {
            Some(attrs) => matches!(&attrs.size, view_types::FileSize::Exact(_)),
            None => true,
        };
        (meta.st_size(), exact)
    }

    fn entry_meta(kind: &wit_types::EntryKind) -> EntryMeta {
        wit_protocol::entry_meta_from_kind(kind)
    }

    fn probe_ranged_attrs(
        &self,
        runtime: &Arc<Runtime>,
        path: &ProtocolPath,
    ) -> Option<FileAttrsCache> {
        let opened = match self
            .rt
            .block_on(runtime.namespace().open_file(path.as_str()))
        {
            Ok(opened) => opened,
            Err(RuntimeError::ProviderError(error))
                if matches!(
                    error.kind,
                    wit_types::ErrorKind::InvalidInput | wit_types::ErrorKind::NotFound
                ) =>
            {
                return None;
            },
            Err(RuntimeError::ProviderError(error)) => {
                tracing::warn!(
                    path = %path,
                    kind = ?error.kind,
                    retryable = error.retryable,
                    message = %error.message,
                    "NFS ranged attr probe failed"
                );
                return None;
            },
            Err(error) => {
                tracing::warn!(path = %path, error = %error, "NFS ranged attr probe failed");
                return None;
            },
        };

        let attrs = FileAttrsCache {
            size: wit_protocol::file_size_from_wit(opened.attrs.size),
            bytes: view_types::ByteSource::Deferred(view_types::ReadMode::Ranged),
            stability: wit_protocol::stability_from_wit(opened.attrs.stability),
            version_token: opened.attrs.version_token,
        };
        if let Err(error) = attrs.validate() {
            tracing::warn!(path = %path, error, "NFS ranged attr probe returned invalid attrs");
            let _ = runtime.call_close_file(opened.handle);
            return None;
        }
        if let Err(error) = runtime.call_close_file(opened.handle) {
            tracing::warn!(path = %path, error = %error, "NFS ranged attr probe close failed");
        }
        Some(attrs)
    }

    fn meta_with_ranged_probe(
        &self,
        runtime: &Arc<Runtime>,
        child_path: &ProtocolPath,
        kind: &wit_types::EntryKind,
    ) -> EntryMeta {
        self.promote_static_placeholder_meta(runtime, child_path, Self::entry_meta(kind))
    }

    fn promote_static_placeholder_meta(
        &self,
        runtime: &Arc<Runtime>,
        child_path: &ProtocolPath,
        mut meta: EntryMeta,
    ) -> EntryMeta {
        if meta
            .attrs
            .as_ref()
            .is_some_and(Self::is_static_file_placeholder)
            && let Some(attrs) = self.probe_ranged_attrs(runtime, child_path)
        {
            frontend::cache_file_metadata(runtime, child_path, attrs.clone());
            meta = EntryMeta::file(attrs);
        }
        meta
    }

    fn is_static_file_placeholder(attrs: &FileAttrsCache) -> bool {
        matches!(attrs.size, view_types::FileSize::Unknown)
            && matches!(
                attrs.bytes,
                view_types::ByteSource::Deferred(view_types::ReadMode::Full)
            )
    }

    fn provider_status(error: &ProviderError) -> Status {
        match ProviderFsError::from_provider(error) {
            ProviderFsError::NotFound => Status::NoEnt,
            ProviderFsError::NotDirectory => Status::NotDir,
            ProviderFsError::IsDirectory => Status::IsDir,
            ProviderFsError::Access => Status::Access,
            ProviderFsError::InvalidInput => Status::Invalid,
            ProviderFsError::TooLarge | ProviderFsError::Retry | ProviderFsError::Io => Status::Io,
        }
    }

    fn expected_negative_probe(name: &str) -> bool {
        name == ".DS_Store" || name.starts_with("._")
    }

    fn lookup_from_cached_dirents(
        &self,
        scope: u64,
        mount_name: &str,
        parent_path: &ProtocolPath,
        parent: u64,
        name: &Segment,
        runtime: &Arc<Runtime>,
    ) -> Option<StatusResult<u64>> {
        let record = frontend::cache_get(runtime, parent_path, RecordKind::Dirents, None)?;
        let lookup = frontend::cached_dirent_lookup(&record, name.as_str())?;
        if matches!(lookup, LookupCacheHit::Negative) {
            return None;
        }
        let path = parent_path.join_segment(name);
        Some(self.resolve_cached_lookup(scope, mount_name, &path, parent, lookup, Some(runtime)))
    }

    fn lookup_from_cached_lookup(
        &self,
        scope: u64,
        mount_name: &str,
        child_path: &ProtocolPath,
        parent: u64,
        runtime: &Arc<Runtime>,
    ) -> Option<StatusResult<u64>> {
        let record = frontend::cache_get(runtime, child_path, RecordKind::Lookup, None)?;
        let lookup = frontend::cached_lookup_record(&record)?;
        Some(self.resolve_cached_lookup(
            scope,
            mount_name,
            child_path,
            parent,
            lookup,
            Some(runtime),
        ))
    }

    fn resolve_cached_lookup(
        &self,
        scope: u64,
        mount_name: &str,
        child_path: &ProtocolPath,
        parent: u64,
        lookup: LookupCacheHit,
        runtime: Option<&Arc<Runtime>>,
    ) -> StatusResult<u64> {
        match lookup {
            LookupCacheHit::Negative => Err(Status::NoEnt),
            LookupCacheHit::Positive(mut meta) => {
                self.negative_lookups
                    .remove(&PathKey::new(mount_name, child_path.as_str()));
                if let Some(runtime) = runtime
                    && meta
                        .attrs
                        .as_ref()
                        .is_some_and(Self::is_static_file_placeholder)
                {
                    meta = self.promote_static_placeholder_meta(runtime, child_path, meta);
                }
                let kind = Self::meta_kind(&meta);
                let (size, size_exact) = Self::meta_size(&meta);
                Ok(self.get_or_alloc(EntrySeed {
                    scope,
                    mount_name,
                    path: child_path,
                    parent,
                    kind,
                    size,
                    size_exact,
                    attrs: meta.attrs,
                    backing_path: None,
                }))
            },
        }
    }

    fn lookup_via_provider(
        &self,
        scope: u64,
        mount_name: &str,
        parent_path: &ProtocolPath,
        parent: u64,
        name: &Segment,
        runtime: &Arc<Runtime>,
    ) -> StatusResult<u64> {
        let child_path = parent_path.join_segment(name);
        match self.rt.block_on(runtime.namespace().lookup_child(
            parent_path.as_str(),
            name.as_str(),
            None,
        )) {
            Ok(LookupOutcome::Subtree(tree_ref)) => self.allocate_provider_subtree(
                scope,
                mount_name,
                &child_path,
                parent,
                runtime,
                tree_ref,
            ),
            Ok(LookupOutcome::Entry(entry)) => Ok(self.allocate_provider_entry(
                scope,
                mount_name,
                &child_path,
                parent,
                runtime,
                &entry,
            )),
            Ok(LookupOutcome::NotFound) => {
                if Self::expected_negative_probe(name.as_str()) {
                    self.negative_lookups
                        .insert(PathKey::new(mount_name, child_path.as_str()), ());
                }
                let payload = cache::LookupPayload::Negative;
                if let Some(encoded) = payload.serialize() {
                    frontend::cache_put(
                        runtime,
                        &child_path,
                        RecordKind::Lookup,
                        None,
                        &CacheRecord::new(RecordKind::Lookup, encoded),
                    );
                }
                Err(Status::NoEnt)
            },
            Err(RuntimeError::ProviderError(error)) => {
                if ProviderFsError::from_provider(&error) == ProviderFsError::NotFound
                    && Self::expected_negative_probe(name.as_str())
                {
                    self.negative_lookups
                        .insert(PathKey::new(mount_name, child_path.as_str()), ());
                    tracing::debug!(
                        op = "lookup",
                        mount = %mount_name,
                        parent = %parent_path,
                        name = %name,
                        "NFS expected negative lookup probe"
                    );
                } else {
                    tracing::warn!(
                        op = "lookup",
                        mount = %mount_name,
                        parent = %parent_path,
                        name = %name,
                        kind = ?error.kind,
                        retryable = error.retryable,
                        message = %error.message,
                        "NFS provider lookup failed"
                    );
                }
                Err(Self::provider_status(&error))
            },
            Err(error) => {
                tracing::warn!(
                    op = "lookup",
                    mount = %mount_name,
                    parent = %parent_path,
                    name = %name,
                    error = %error,
                    "NFS runtime lookup failed"
                );
                Err(Status::Io)
            },
        }
    }

    fn allocate_provider_subtree(
        &self,
        scope: u64,
        mount_name: &str,
        child_path: &ProtocolPath,
        parent: u64,
        runtime: &Arc<Runtime>,
        tree_ref: u64,
    ) -> StatusResult<u64> {
        let Some(real_root) = runtime.resolve_tree_ref(tree_ref) else {
            return Err(Status::Io);
        };
        self.negative_lookups
            .remove(&PathKey::new(mount_name, child_path.as_str()));
        Ok(self.get_or_alloc(EntrySeed {
            scope,
            mount_name,
            path: child_path,
            parent,
            kind: NodeKind::Directory,
            size: 0,
            size_exact: true,
            attrs: None,
            backing_path: Some(real_root),
        }))
    }

    fn allocate_provider_entry(
        &self,
        scope: u64,
        mount_name: &str,
        child_path: &ProtocolPath,
        parent: u64,
        runtime: &Arc<Runtime>,
        entry: &LookupEntry,
    ) -> u64 {
        let meta = self.promote_static_placeholder_meta(runtime, child_path, entry.meta().clone());
        let kind = Self::meta_kind(&meta);
        let (size, size_exact) = Self::meta_size(&meta);
        self.negative_lookups
            .remove(&PathKey::new(mount_name, child_path.as_str()));
        self.get_or_alloc(EntrySeed {
            scope,
            mount_name,
            path: child_path,
            parent,
            kind,
            size,
            size_exact,
            attrs: meta.attrs,
            backing_path: None,
        })
    }

    fn lookup_backing_child(
        &self,
        scope: u64,
        mount_name: &str,
        parent_path: &ProtocolPath,
        parent: u64,
        name: &Segment,
        root: &FsPath,
    ) -> StatusResult<u64> {
        let child = root.join(name.as_str());
        let metadata = std::fs::symlink_metadata(&child).map_err(|_| Status::NoEnt)?;
        let kind = Self::backing_kind(&metadata)?;
        let child_path = parent_path.join_segment(name);
        self.negative_lookups
            .remove(&PathKey::new(mount_name, child_path.as_str()));
        Ok(self.get_or_alloc(EntrySeed {
            scope,
            mount_name,
            path: &child_path,
            parent,
            kind,
            size: metadata.len(),
            size_exact: true,
            attrs: None,
            backing_path: Some(child),
        }))
    }

    fn lookup_from_parent_subtree(
        &self,
        scope: u64,
        mount_name: &str,
        parent_path: &ProtocolPath,
        parent: u64,
        name: &Segment,
        runtime: &Arc<Runtime>,
    ) -> Option<StatusResult<u64>> {
        if parent_path.is_root() || Self::expected_negative_probe(name.as_str()) {
            return None;
        }

        let cached_dirents = frontend::cache_get(runtime, parent_path, RecordKind::Dirents, None)
            .and_then(|record| cache::DirentsPayload::deserialize(&record.payload));
        let cached_validator = cached_dirents
            .as_ref()
            .and_then(|dirents| dirents.validator.clone());

        match self.rt.block_on(runtime.namespace().list_children(
            parent_path.as_str(),
            cached_validator,
            None,
            None,
        )) {
            Ok(ListChildrenResult::Subtree(tree_ref)) => {
                let Some(real_root) = runtime.resolve_tree_ref(tree_ref) else {
                    return Some(Err(Status::Io));
                };
                if let Some(mut entry) = self.entries.get_mut(&parent) {
                    entry.backing_path = Some(real_root.clone());
                    entry.attrs = None;
                    entry.size_exact = true;
                }
                Some(self.lookup_backing_child(
                    scope,
                    mount_name,
                    parent_path,
                    parent,
                    name,
                    &real_root,
                ))
            },
            Ok(_) => None,
            Err(error) => {
                tracing::debug!(
                    op = "lookup",
                    mount = %mount_name,
                    parent = %parent_path,
                    name = %name,
                    error = %error,
                    "NFS parent subtree materialization probe failed"
                );
                None
            },
        }
    }

    fn readdir_from_dirents(
        &self,
        scope: u64,
        mount_name: &str,
        path: &ProtocolPath,
        parent: u64,
        dirents: &cache::DirentsPayload,
    ) -> DirListing {
        let entries = dirents
            .entries
            .iter()
            .filter_map(|entry| {
                let Ok(name) = Segment::try_from(entry.name.as_str()) else {
                    return None;
                };
                let child_path = path.join_segment(&name);
                self.negative_lookups
                    .remove(&PathKey::new(mount_name, child_path.as_str()));
                let kind = Self::meta_kind(&entry.meta);
                let (size, size_exact) = Self::meta_size(&entry.meta);
                let id = self.get_or_alloc(EntrySeed {
                    scope,
                    mount_name,
                    path: &child_path,
                    parent,
                    kind,
                    size,
                    size_exact,
                    attrs: entry.meta.attrs.clone(),
                    backing_path: None,
                });
                let attr = self.attr(id).unwrap_or(Attr {
                    id,
                    parent,
                    kind,
                    size,
                    mode: kind.mode(),
                    change: id,
                    mtime_sec: 0,
                });
                Some(DirEntry {
                    id,
                    name: entry.name.clone(),
                    attr,
                })
            })
            .collect();
        DirListing {
            entries,
            exhaustive: dirents.exhaustive,
        }
    }

    fn readdir_backing(
        &self,
        scope: u64,
        mount_name: &str,
        path: &ProtocolPath,
        parent: u64,
        root: &FsPath,
    ) -> StatusResult<DirListing> {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(root).map_err(|_| Status::Io)? {
            let entry = entry.map_err(|_| Status::Io)?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Ok(name) = Segment::try_from(name) else {
                continue;
            };
            let child_path = path.join_segment(&name);
            let backing_path = entry.path();
            let metadata = std::fs::symlink_metadata(&backing_path).map_err(|_| Status::Io)?;
            let Ok(kind) = Self::backing_kind(&metadata) else {
                continue;
            };
            let id = self.get_or_alloc(EntrySeed {
                scope,
                mount_name,
                path: &child_path,
                parent,
                kind,
                size: metadata.len(),
                size_exact: true,
                attrs: None,
                backing_path: Some(backing_path),
            });
            entries.push(DirEntry {
                id,
                name: name.as_str().to_string(),
                attr: Self::attr_from_metadata(id, parent, &metadata)?,
            });
        }
        // Sorting happens centrally in `handle_readdir`; a local pre-sort here
        // would diverge silently if that policy changes.
        Ok(DirListing {
            entries,
            exhaustive: true,
        })
    }

    fn readdir_via_provider(
        &self,
        scope: u64,
        mount_name: &str,
        path: &ProtocolPath,
        parent: u64,
        runtime: &Arc<Runtime>,
    ) -> StatusResult<DirListing> {
        let cached_dirents = frontend::cache_get(runtime, path, RecordKind::Dirents, None)
            .and_then(|record| cache::DirentsPayload::deserialize(&record.payload));
        let cached_validator = cached_dirents
            .as_ref()
            .and_then(|dirents| dirents.validator.clone());

        match self.rt.block_on(runtime.namespace().list_children(
            path.as_str(),
            cached_validator,
            None,
            None,
        )) {
            Ok(ListChildrenResult::Unchanged) => {
                let Some(dirents) = cached_dirents else {
                    tracing::warn!(
                        op = "readdir",
                        mount = %mount_name,
                        path = %path,
                        "NFS provider returned unchanged with no cached listing"
                    );
                    return Err(Status::Io);
                };
                Ok(self.readdir_from_dirents(scope, mount_name, path, parent, &dirents))
            },
            Ok(ListChildrenResult::Subtree(tree_ref)) => {
                let Some(real_root) = runtime.resolve_tree_ref(tree_ref) else {
                    return Err(Status::Io);
                };
                if let Some(mut entry) = self.entries.get_mut(&parent) {
                    entry.backing_path = Some(real_root.clone());
                }
                self.readdir_backing(scope, mount_name, path, parent, &real_root)
            },
            Ok(ListChildrenResult::Entries(listing)) => {
                let mut records = Vec::with_capacity(listing.entries.len());
                for entry in &listing.entries {
                    let Ok(name) = Segment::try_from(entry.name.as_str()) else {
                        continue;
                    };
                    let child_path = path.join_segment(&name);
                    records.push(cache::DirentRecord {
                        name: entry.name.clone(),
                        meta: self.meta_with_ranged_probe(runtime, &child_path, &entry.kind),
                    });
                }
                records.sort_by(|left, right| left.name.cmp(&right.name));
                let dirents = cache::DirentsPayload {
                    entries: records,
                    exhaustive: listing.exhaustive && listing.next_cursor.is_none(),
                    validator: listing.validator.clone(),
                    next_cursor: listing
                        .next_cursor
                        .clone()
                        .map(wit_protocol::cached_cursor_from_wit),
                    paginated: listing.next_cursor.is_some(),
                };
                Ok(self.readdir_from_dirents(scope, mount_name, path, parent, &dirents))
            },
            Err(RuntimeError::ProviderError(error)) => {
                tracing::warn!(
                    op = "readdir",
                    mount = %mount_name,
                    path = %path,
                    kind = ?error.kind,
                    retryable = error.retryable,
                    message = %error.message,
                    "NFS provider readdir failed"
                );
                Err(Self::provider_status(&error))
            },
            Err(error) => {
                tracing::warn!(
                    op = "readdir",
                    mount = %mount_name,
                    path = %path,
                    error = %error,
                    "NFS runtime readdir failed"
                );
                Err(Status::Io)
            },
        }
    }

    fn read_provider_file(
        &self,
        id: u64,
        mount_name: &str,
        path: &ProtocolPath,
        attrs: Option<FileAttrsCache>,
        runtime: &Runtime,
    ) -> StatusResult<Vec<u8>> {
        let read_attrs = frontend::cached_file_attrs(runtime, path).or(attrs);
        if let Some(attrs) = read_attrs.as_ref()
            && let Some(bytes) = attrs.inline_bytes()
        {
            let data = bytes.to_vec();
            let attrs = frontend::learned_full_read_attrs(attrs.clone(), data.len());
            if !frontend::full_read_matches_attrs(&attrs, data.len()) {
                tracing::warn!(
                    path = %path,
                    expected = ?attrs.size,
                    actual = data.len(),
                    "cached inline file attrs contradict content length"
                );
                return Err(Status::Io);
            }
            self.promote_file_attrs(id, attrs.clone());
            frontend::cache_file_metadata(runtime, path, attrs);
            return Ok(data);
        }

        let cached_record = read_attrs.as_ref().and_then(|attrs| {
            attrs.durable_cache_aux().and_then(|aux| {
                frontend::cache_get(runtime, path, RecordKind::File, aux.as_deref())
            })
        });
        if let Some(record) = cached_record
            && let Some(payload) = FilePayload::deserialize(&record.payload)
            && read_attrs
                .as_ref()
                .is_none_or(|attrs| frontend::file_payload_matches_attrs(attrs, &payload))
        {
            let data = payload.content;
            let size = u64::try_from(data.len()).unwrap_or(u64::MAX);
            let attrs = read_attrs.map_or_else(
                || frontend::exact_file_attrs(size),
                |attrs| frontend::learned_full_read_attrs(attrs, data.len()),
            );
            self.promote_file_attrs(id, attrs.clone());
            frontend::cache_file_metadata(runtime, path, attrs);
            return Ok(data);
        }

        let content_type = String::new();
        match self.rt.block_on(
            runtime
                .namespace()
                .read_file(path.as_str(), content_type, None),
        ) {
            Ok(result) => {
                let Some(resolved) =
                    frontend::ResolvedRead::from_provider_result(runtime, path, result)
                else {
                    tracing::warn!(path = %path, "NFS read payload could not be resolved");
                    return Err(Status::Io);
                };
                let data = resolved.data;
                let attrs = frontend::learned_full_read_attrs(resolved.attrs, data.len());
                if !frontend::full_read_matches_attrs(&attrs, data.len()) {
                    tracing::warn!(
                        path = %path,
                        expected = ?attrs.size,
                        actual = data.len(),
                        "provider returned bytes that contradict file attrs"
                    );
                    return Err(Status::Io);
                }
                if resolved.cache_rendered_file
                    && let Some((aux, record)) =
                        frontend::durable_file_record(&attrs, &data, resolved.content_type)
                {
                    frontend::cache_put(runtime, path, RecordKind::File, aux.as_deref(), &record);
                }
                self.promote_file_attrs(id, attrs.clone());
                frontend::cache_file_metadata(runtime, path, attrs);
                Ok(data)
            },
            Err(RuntimeError::ProviderError(error)) => {
                tracing::warn!(
                    op = "read",
                    mount = %mount_name,
                    path = %path,
                    kind = ?error.kind,
                    retryable = error.retryable,
                    message = %error.message,
                    "NFS provider read failed"
                );
                Err(Self::provider_status(&error))
            },
            Err(error) => {
                tracing::warn!(
                    op = "read",
                    mount = %mount_name,
                    path = %path,
                    error = %error,
                    "NFS runtime read failed"
                );
                Err(Status::Io)
            },
        }
    }

    fn read_ranged_state(
        &self,
        id: u64,
        ranged: &RangedOpen,
        offset: u64,
        count: u32,
    ) -> StatusResult<OpenRead> {
        let runtime = self
            .runtime_for_mount(&ranged.mount_name)
            .ok_or(Status::NoEnt)?;
        let count = count.min(MAX_NFS_READ_BYTES);
        match self.rt.block_on(runtime.namespace().read_chunk(
            ranged.provider_handle,
            offset,
            count,
        )) {
            Ok(chunk) => {
                if chunk.content.len() > count as usize {
                    tracing::warn!(
                        path = ranged.path.as_str(),
                        requested = count,
                        returned = chunk.content.len(),
                        "provider returned oversized ranged chunk"
                    );
                    return Err(Status::Io);
                }
                let content_len = u64::try_from(chunk.content.len()).map_err(|_| Status::Io)?;
                let observed_end = offset.checked_add(content_len).ok_or(Status::Io)?;
                if let view_types::FileSize::Exact(size) = ranged.attrs.size
                    && observed_end > size
                {
                    tracing::warn!(
                        path = ranged.path.as_str(),
                        offset,
                        returned = chunk.content.len(),
                        size,
                        "provider returned ranged bytes beyond exact file size"
                    );
                    return Err(Status::Io);
                }
                if chunk.eof {
                    if let Err(error) = ranged.attrs.validate_observed_size(observed_end) {
                        tracing::warn!(
                            path = ranged.path.as_str(),
                            error,
                            "provider returned ranged EOF that contradicts file attrs"
                        );
                        return Err(Status::Io);
                    }
                    if let Some(attrs) =
                        frontend::learned_ranged_eof_attrs(ranged.attrs.clone(), observed_end)
                    {
                        self.promote_file_attrs(id, attrs.clone());
                        frontend::cache_file_metadata(&runtime, &ranged.path, attrs);
                    }
                }
                Ok(OpenRead {
                    id,
                    data: chunk.content,
                    eof: chunk.eof,
                })
            },
            Err(RuntimeError::ProviderError(error)) => {
                tracing::warn!(
                    op = "read",
                    mount = %ranged.mount_name,
                    path = %ranged.path,
                    kind = ?error.kind,
                    retryable = error.retryable,
                    message = %error.message,
                    "NFS provider ranged read failed"
                );
                Err(Self::provider_status(&error))
            },
            Err(error) => {
                tracing::warn!(
                    op = "read",
                    mount = %ranged.mount_name,
                    path = %ranged.path,
                    error = %error,
                    "NFS runtime ranged read failed"
                );
                Err(Status::Io)
            },
        }
    }

    fn read_backing_state(
        backing: &BackingOpen,
        offset: u64,
        count: u32,
    ) -> StatusResult<OpenRead> {
        let metadata =
            std::fs::symlink_metadata(&backing.backing_path).map_err(|_| Status::Stale)?;
        if metadata.file_type().is_symlink() {
            return Err(Status::Symlink);
        }
        if metadata.is_dir() {
            return Err(Status::IsDir);
        }
        if !metadata.is_file() {
            return Err(Status::Invalid);
        }

        let count = usize::try_from(count.min(MAX_NFS_READ_BYTES)).map_err(|_| Status::Io)?;
        let mut file = std::fs::File::open(&backing.backing_path).map_err(|_| Status::Io)?;
        file.seek(SeekFrom::Start(offset)).map_err(|_| Status::Io)?;
        let mut data = vec![0; count];
        let read = file.read(&mut data).map_err(|_| Status::Io)?;
        data.truncate(read);
        let read_end = offset
            .checked_add(u64::try_from(read).map_err(|_| Status::Io)?)
            .ok_or(Status::Io)?;
        Ok(OpenRead {
            id: backing.id,
            data,
            eof: read_end >= metadata.len(),
        })
    }

    /// Refuse the open up-front when we can already prove it would blow the
    /// materialization budget, or when the declared bytes contract leaves
    /// the post-read size unbounded. Ranged opens stream chunks through a
    /// provider handle and bypass this check; backing files and full-mode
    /// projected files share the same byte-cap policy.
    fn enforce_materialize_cap(
        mount_name: &str,
        path: &ProtocolPath,
        attrs: Option<&FileAttrsCache>,
        backing_path: Option<&FsPath>,
    ) -> StatusResult<()> {
        if backing_path.is_some() {
            return Ok(());
        }
        // Exact full-mode projections can be rejected before reading. Non-exact
        // full-mode projections are still materialized below and checked
        // against the same cap after bytes are available, matching FUSE open
        // behavior for static-shape placeholder attrs.
        if let Some(projected_attrs) = attrs
            && matches!(
                projected_attrs.bytes,
                view_types::ByteSource::Deferred(view_types::ReadMode::Full)
            )
        {
            match projected_attrs.size {
                view_types::FileSize::Exact(declared) if declared > OPEN_MATERIALIZE_MAX_BYTES => {
                    tracing::warn!(
                        op = "open",
                        mount = %mount_name,
                        path = %path,
                        size = declared,
                        cap = OPEN_MATERIALIZE_MAX_BYTES,
                        "rejecting full-mode open: declared exact size exceeds materialize cap"
                    );
                    return Err(Status::Resource);
                },
                view_types::FileSize::Exact(_)
                | view_types::FileSize::NonZero
                | view_types::FileSize::Unknown => {},
            }
        }
        Ok(())
    }

    fn close_ranged_provider_handle(&self, ranged: &RangedOpen) {
        if let Some(runtime) = self.runtime_for_mount(&ranged.mount_name)
            && let Err(error) = runtime.call_close_file(ranged.provider_handle)
        {
            tracing::warn!(
                mount = %ranged.mount_name,
                path = %ranged.path,
                error = %error,
                "NFS runtime ranged close failed"
            );
        }
    }

    fn promote_ranged_attrs_for_open(
        &self,
        id: u64,
        mount_name: &str,
        path: &ProtocolPath,
        attrs: &mut Option<FileAttrsCache>,
        backing_path: Option<&FsPath>,
    ) {
        if backing_path.is_some()
            || attrs
                .as_ref()
                .is_some_and(|attrs| !Self::is_static_file_placeholder(attrs))
        {
            return;
        }
        let Some(runtime) = self.runtime_for_mount(mount_name) else {
            return;
        };
        let Some(probed_attrs) = self.probe_ranged_attrs(&runtime, path) else {
            return;
        };
        self.promote_file_attrs(id, probed_attrs.clone());
        frontend::cache_file_metadata(&runtime, path, probed_attrs.clone());
        *attrs = Some(probed_attrs);
    }

    fn open_ranged_state(
        &self,
        seed: OpenSeed,
        mount_name: String,
        path: ProtocolPath,
        projected_attrs: &FileAttrsCache,
        runtime: &Arc<Runtime>,
    ) -> StatusResult<OpenResult> {
        match self
            .rt
            .block_on(runtime.namespace().open_file(path.as_str()))
        {
            Ok(opened) => {
                let opened_attrs =
                    match frontend::opened_file_attrs(Some(projected_attrs), &opened.attrs) {
                        Ok(attrs) => attrs,
                        Err(error) => {
                            tracing::warn!(path = %path, error, "open-file returned invalid attrs");
                            let _ = runtime.call_close_file(opened.handle);
                            return Err(Status::Io);
                        },
                    };
                self.promote_file_attrs(seed.inode, opened_attrs.clone());
                frontend::cache_file_metadata(runtime, &path, opened_attrs.clone());
                let attr = match self.attr(seed.inode) {
                    Ok(attr) => attr,
                    Err(status) => {
                        let _ = runtime.call_close_file(opened.handle);
                        return Err(status);
                    },
                };
                let stateid = self.opens.open(seed);
                self.ranged_opens.insert(
                    stateid.other(),
                    RangedOpen {
                        mount_name,
                        path,
                        provider_handle: opened.handle,
                        attrs: opened_attrs,
                    },
                );
                Ok(OpenResult { stateid, attr })
            },
            Err(RuntimeError::ProviderError(error)) => {
                tracing::warn!(
                    op = "open",
                    mount = %mount_name,
                    path = %path,
                    kind = ?error.kind,
                    retryable = error.retryable,
                    message = %error.message,
                    "NFS provider ranged open failed"
                );
                Err(Self::provider_status(&error))
            },
            Err(error) => {
                tracing::warn!(
                    op = "open",
                    mount = %mount_name,
                    path = %path,
                    error = %error,
                    "NFS runtime ranged open failed"
                );
                Err(Status::Io)
            },
        }
    }
}

impl ReadOnlyExport for Export {
    fn root(&self) -> u64 {
        ROOT_ID
    }

    fn attr(&self, id: u64) -> StatusResult<Attr> {
        let entry = self.entries.get(&id).ok_or(Status::Stale)?;
        let mount_name = entry.mount_name.clone();
        let backing_path = entry.backing_path.clone();
        drop(entry);

        if !mount_name.is_empty() {
            self.drain_invalidations_for_mount(&mount_name);
        }

        let mut entry = self.entries.get_mut(&id).ok_or(Status::Stale)?;
        entry.last_seen = Instant::now();
        if let Some(path) = &backing_path {
            let metadata = std::fs::symlink_metadata(path).map_err(|_| Status::Stale)?;
            Self::attr_from_metadata(id, entry.parent, &metadata)
        } else {
            Ok(Self::attr_from_entry(id, &entry))
        }
    }

    fn lookup(&self, parent: u64, name: &str) -> StatusResult<u64> {
        let name = Segment::try_from(name).map_err(|_| Status::Invalid)?;

        if (parent == ROOT_ID || parent == EXPORT_ROOT_ID)
            && self.root_mount.is_none()
            && self.registry.get(name.as_str()).is_some()
        {
            return Ok(self.get_or_alloc(EntrySeed {
                scope: parent,
                mount_name: name.as_str(),
                path: &ProtocolPath::root(),
                parent,
                kind: NodeKind::Directory,
                size: 0,
                size_exact: true,
                attrs: None,
                backing_path: None,
            }));
        }

        if parent == ROOT_ID && name.as_str() == NFS_EXPORT_NAME {
            return Ok(EXPORT_ROOT_ID);
        }

        let mut parent_entry = self.entries.get_mut(&parent).ok_or(Status::Stale)?;
        if parent_entry.kind != NodeKind::Directory {
            return Err(Status::NotDir);
        }
        parent_entry.last_seen = Instant::now();
        let mount_name = parent_entry.mount_name.clone();
        let parent_path = parent_entry.path.clone();
        let scope = parent_entry.scope;
        let backing_path = parent_entry.backing_path.clone();
        drop(parent_entry);

        self.drain_invalidations_for_mount(&mount_name);
        // Invalidations may have just removed `parent` from the object table.
        // Re-confirm before binding a new child to it, otherwise the child
        // would inherit an orphan parent inode that fails Status::Stale on
        // every subsequent attr/lookupp.
        if !self.entries.contains_key(&parent) {
            return Err(Status::Stale);
        }

        if let Some(root) = backing_path {
            return self.lookup_backing_child(
                scope,
                &mount_name,
                &parent_path,
                parent,
                &name,
                &root,
            );
        }

        let runtime = self.runtime_for_mount(&mount_name).ok_or(Status::NoEnt)?;
        let child_path = parent_path.join_segment(&name);
        if let Some(result) = self.lookup_from_cached_dirents(
            scope,
            &mount_name,
            &parent_path,
            parent,
            &name,
            &runtime,
        ) {
            return result;
        }
        if let Some(result) =
            self.lookup_from_cached_lookup(scope, &mount_name, &child_path, parent, &runtime)
        {
            return result;
        }
        if Self::expected_negative_probe(name.as_str())
            && self
                .negative_lookups
                .contains_key(&PathKey::new(&mount_name, child_path.as_str()))
        {
            return Err(Status::NoEnt);
        }
        let result =
            self.lookup_via_provider(scope, &mount_name, &parent_path, parent, &name, &runtime);
        if matches!(result, Err(Status::NoEnt))
            && let Some(subtree_result) = self.lookup_from_parent_subtree(
                scope,
                &mount_name,
                &parent_path,
                parent,
                &name,
                &runtime,
            )
        {
            return subtree_result;
        }
        result
    }

    fn readdir(&self, id: u64) -> StatusResult<DirListing> {
        if (id == ROOT_ID || id == EXPORT_ROOT_ID) && self.root_mount.is_none() {
            let mut mounts = self.registry.mounts();
            mounts.sort();
            let entries = mounts
                .into_iter()
                .map(|mount| {
                    let child = self.get_or_alloc(EntrySeed {
                        scope: id,
                        mount_name: &mount,
                        path: &ProtocolPath::root(),
                        parent: id,
                        kind: NodeKind::Directory,
                        size: 0,
                        size_exact: true,
                        attrs: None,
                        backing_path: None,
                    });
                    DirEntry {
                        id: child,
                        name: mount,
                        attr: self.attr(child).expect("fresh mount attr"),
                    }
                })
                .collect();
            return Ok(DirListing {
                entries,
                exhaustive: true,
            });
        }

        let mut entry = self.entries.get_mut(&id).ok_or(Status::Stale)?;
        if entry.kind != NodeKind::Directory {
            return Err(Status::NotDir);
        }
        entry.last_seen = Instant::now();
        let mount_name = entry.mount_name.clone();
        let path = entry.path.clone();
        let scope = entry.scope;
        let backing_path = entry.backing_path.clone();
        drop(entry);

        self.drain_invalidations_for_mount(&mount_name);
        if !self.entries.contains_key(&id) {
            return Err(Status::Stale);
        }

        if let Some(root) = backing_path {
            return self.readdir_backing(scope, &mount_name, &path, id, &root);
        }

        let runtime = self.runtime_for_mount(&mount_name).ok_or(Status::NoEnt)?;
        if let Some(record) = frontend::cache_get(&runtime, &path, RecordKind::Dirents, None)
            && let Some(dirents) = frontend::cached_exhaustive_dirents(&record)
        {
            return Ok(self.readdir_from_dirents(scope, &mount_name, &path, id, &dirents));
        }

        self.readdir_via_provider(scope, &mount_name, &path, id, &runtime)
    }

    fn read(&self, id: u64) -> StatusResult<Vec<u8>> {
        let mut entry = self.entries.get_mut(&id).ok_or(Status::Stale)?;
        if entry.kind == NodeKind::Directory {
            return Err(Status::IsDir);
        }
        if entry.kind == NodeKind::Symlink {
            return Err(Status::Invalid);
        }
        entry.last_seen = Instant::now();
        let mount_name = entry.mount_name.clone();
        let path = entry.path.clone();
        let attrs = entry.attrs.clone();
        let backing_path = entry.backing_path.clone();
        drop(entry);

        self.drain_invalidations_for_mount(&mount_name);
        if !self.entries.contains_key(&id) {
            return Err(Status::Stale);
        }

        if let Some(backing_path) = backing_path {
            let metadata = std::fs::symlink_metadata(&backing_path).map_err(|_| Status::Stale)?;
            if metadata.file_type().is_symlink() {
                return Err(Status::Symlink);
            }
            if metadata.is_dir() {
                return Err(Status::IsDir);
            }
            if !metadata.is_file() {
                return Err(Status::Invalid);
            }
            return std::fs::read(backing_path).map_err(|_| Status::Io);
        }

        let runtime = self.runtime_for_mount(&mount_name).ok_or(Status::NoEnt)?;
        self.read_provider_file(id, &mount_name, &path, attrs, &runtime)
    }

    fn readlink(&self, id: u64) -> StatusResult<Vec<u8>> {
        let mut entry = self.entries.get_mut(&id).ok_or(Status::Stale)?;
        if entry.kind != NodeKind::Symlink {
            return Err(Status::Invalid);
        }
        entry.last_seen = Instant::now();
        let mount_name = entry.mount_name.clone();
        let Some(path) = entry.backing_path.clone() else {
            return Err(Status::Invalid);
        };
        drop(entry);
        self.drain_invalidations_for_mount(&mount_name);
        if !self.entries.contains_key(&id) {
            return Err(Status::Stale);
        }
        std::fs::read_link(path)
            .map(|target| target.as_os_str().as_encoded_bytes().to_vec())
            .map_err(|_| Status::Io)
    }

    fn open_state(
        &self,
        generation: u64,
        id: u64,
        clientid: u64,
        access: u32,
    ) -> StatusResult<OpenResult> {
        let mut entry = self.entries.get_mut(&id).ok_or(Status::Stale)?;
        entry.last_seen = Instant::now();
        let mount_name = entry.mount_name.clone();
        let path = entry.path.clone();
        let mut attrs = entry.attrs.clone();
        let backing_path = entry.backing_path.clone();
        drop(entry);
        self.drain_invalidations_for_mount(&mount_name);
        if !self.entries.contains_key(&id) {
            return Err(Status::Stale);
        }

        self.promote_ranged_attrs_for_open(
            id,
            &mount_name,
            &path,
            &mut attrs,
            backing_path.as_deref(),
        );

        Self::enforce_materialize_cap(&mount_name, &path, attrs.as_ref(), backing_path.as_deref())?;

        if backing_path.is_none()
            && let Some(projected_attrs) = attrs.as_ref()
            && matches!(
                projected_attrs.bytes,
                view_types::ByteSource::Deferred(view_types::ReadMode::Ranged)
            )
        {
            let runtime = self.runtime_for_mount(&mount_name).ok_or(Status::NoEnt)?;
            return self.open_ranged_state(
                OpenSeed {
                    generation,
                    inode: id,
                    clientid,
                    access,
                    materialized_bytes: Vec::new(),
                },
                mount_name,
                path,
                projected_attrs,
                &runtime,
            );
        }

        if let Some(backing_path) = backing_path {
            let attr = self.attr(id)?;
            let stateid = self.opens.open(OpenSeed {
                generation,
                inode: id,
                clientid,
                access,
                materialized_bytes: Vec::new(),
            });
            self.backing_opens.insert(
                stateid.other(),
                BackingOpen {
                    id,
                    mount_name,
                    path,
                    backing_path,
                },
            );
            return Ok(OpenResult { stateid, attr });
        }

        let data = self.materialize_for_open(id)?;
        // Belt-and-braces guard: providers that declared non-exact sizes can
        // still return arbitrarily large payloads. The pre-check above only
        // catches declared exact sizes.
        if u64::try_from(data.len()).unwrap_or(u64::MAX) > OPEN_MATERIALIZE_MAX_BYTES {
            tracing::warn!(
                op = "open",
                mount = %mount_name,
                path = %path,
                size = data.len(),
                cap = OPEN_MATERIALIZE_MAX_BYTES,
                "rejecting full-mode open: observed payload exceeds materialize cap"
            );
            return Err(Status::Resource);
        }
        let attr = self.attr(id)?;
        let stateid = self.opens.open(OpenSeed {
            generation,
            inode: id,
            clientid,
            access,
            materialized_bytes: data,
        });
        Ok(OpenResult { stateid, attr })
    }

    fn validate_state(&self, stateid: StateId) -> StatusResult<()> {
        match self.opens.touch(stateid) {
            Ok(_) => Ok(()),
            Err(Status::Expired) => {
                if let Some((_, ranged)) = self.ranged_opens.remove(&stateid.other()) {
                    self.close_ranged_provider_handle(&ranged);
                }
                self.backing_opens.remove(&stateid.other());
                Err(Status::Expired)
            },
            Err(status) => Err(status),
        }
    }

    fn read_state(&self, stateid: StateId, offset: u64, count: u32) -> StatusResult<OpenRead> {
        if let Some(backing) = self
            .backing_opens
            .get(&stateid.other())
            .map(|entry| entry.clone())
        {
            self.drain_invalidations_for_mount(&backing.mount_name);
            if !self.backing_opens.contains_key(&stateid.other()) {
                return Err(Status::BadStateId);
            }
            let info = match self.opens.read_info(stateid) {
                Ok(info) => info,
                Err(Status::Expired) => {
                    self.backing_opens.remove(&stateid.other());
                    return Err(Status::Expired);
                },
                Err(status) => return Err(status),
            };
            self.touch_entry(info.id);
            return Self::read_backing_state(&backing, offset, count);
        }

        if let Some(ranged) = self
            .ranged_opens
            .get(&stateid.other())
            .map(|entry| entry.clone())
        {
            self.drain_invalidations_for_mount(&ranged.mount_name);
            if !self.ranged_opens.contains_key(&stateid.other()) {
                return Err(Status::BadStateId);
            }
            let info = match self.opens.read_info(stateid) {
                Ok(info) => info,
                Err(Status::Expired) => {
                    self.close_ranged_provider_handle(&ranged);
                    self.ranged_opens.remove(&stateid.other());
                    return Err(Status::Expired);
                },
                Err(status) => return Err(status),
            };
            self.touch_entry(info.id);
            return self.read_ranged_state(info.id, &ranged, offset, count);
        }

        let info = self.opens.touch(stateid)?;
        let mut entry = self.entries.get_mut(&info.id).ok_or(Status::Stale)?;
        entry.last_seen = Instant::now();
        let mount_name = entry.mount_name.clone();
        drop(entry);
        self.drain_invalidations_for_mount(&mount_name);
        self.opens.read(stateid, offset, count)
    }

    fn close_state(&self, stateid: StateId) -> StatusResult<StateId> {
        match self.opens.close(stateid) {
            Ok(next_stateid) => {
                if let Some((_, ranged)) = self.ranged_opens.remove(&stateid.other()) {
                    self.close_ranged_provider_handle(&ranged);
                }
                self.backing_opens.remove(&stateid.other());
                Ok(next_stateid)
            },
            Err(Status::Expired) => {
                if let Some((_, ranged)) = self.ranged_opens.remove(&stateid.other()) {
                    self.close_ranged_provider_handle(&ranged);
                }
                self.backing_opens.remove(&stateid.other());
                Err(Status::Expired)
            },
            Err(status) => Err(status),
        }
    }

    fn renew_client(&self, clientid: u64) -> StatusResult<()> {
        self.opens.renew_client(clientid);
        Ok(())
    }

    fn materialize_for_open(&self, id: u64) -> StatusResult<Vec<u8>> {
        self.read(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::runtime::Runtime as TokioRuntime;

    struct EmptyRegistry;

    impl RegistryView for EmptyRegistry {
        fn get(&self, _mount: &str) -> Option<Arc<Runtime>> {
            None
        }

        fn mounts(&self) -> Vec<String> {
            Vec::new()
        }

        fn root_mount_name(&self) -> Option<String> {
            None
        }
    }

    struct TestExport {
        export: Export,
        _runtime: TokioRuntime,
    }

    fn empty_export() -> TestExport {
        let runtime = TokioRuntime::new().expect("tokio runtime");
        let export = Export::with_registry(runtime.handle().clone(), Arc::new(EmptyRegistry));
        TestExport {
            export,
            _runtime: runtime,
        }
    }

    fn attrs(
        size: view_types::FileSize,
        stability: view_types::Stability,
        version_token: Option<&str>,
    ) -> FileAttrsCache {
        FileAttrsCache {
            size,
            bytes: view_types::ByteSource::Deferred(view_types::ReadMode::Full),
            stability,
            version_token: version_token.map(str::to_string),
        }
    }

    fn exact_size(attrs: &FileAttrsCache) -> Option<u64> {
        match attrs.size {
            view_types::FileSize::Exact(size) => Some(size),
            view_types::FileSize::NonZero | view_types::FileSize::Unknown => None,
        }
    }

    fn test_path(name: &str) -> ProtocolPath {
        ProtocolPath::root()
            .join(name)
            .expect("test path segment is valid")
    }

    fn insert_stale_leaf(export: &Export, id: u64) {
        let path = test_path(&format!("stale-{id}"));
        export
            .path_to_inode
            .insert(ObjectKey::new(ROOT_ID, "test", &path), id);
        export.entries.insert(
            id,
            ObjectRecord {
                scope: ROOT_ID,
                mount_name: "test".to_string(),
                path,
                parent: ROOT_ID,
                kind: NodeKind::File,
                size: 1,
                size_exact: true,
                attrs: None,
                backing_path: None,
                last_seen: Instant::now()
                    .checked_sub(ENTRY_IDLE_TTL + Duration::from_secs(1))
                    .expect("test duration is before now"),
            },
        );
    }

    fn insert_full_mode_leaf(
        export: &Export,
        id: u64,
        name: &str,
        declared_size: view_types::FileSize,
    ) {
        let path = test_path(name);
        let attrs = FileAttrsCache {
            size: declared_size,
            bytes: view_types::ByteSource::Deferred(view_types::ReadMode::Full),
            stability: view_types::Stability::Stable,
            version_token: None,
        };
        let recorded_size = match declared_size {
            view_types::FileSize::Exact(n) => n,
            _ => 0,
        };
        export
            .path_to_inode
            .insert(ObjectKey::new(ROOT_ID, "test", &path), id);
        export.entries.insert(
            id,
            ObjectRecord {
                scope: ROOT_ID,
                mount_name: "test".to_string(),
                path,
                parent: ROOT_ID,
                kind: NodeKind::File,
                size: recorded_size,
                size_exact: matches!(declared_size, view_types::FileSize::Exact(_)),
                attrs: Some(attrs),
                backing_path: None,
                last_seen: Instant::now(),
            },
        );
    }

    fn insert_backing_file(export: &Export, id: u64, name: &str, backing: PathBuf, size: u64) {
        let path = test_path(name);
        export
            .path_to_inode
            .insert(ObjectKey::new(ROOT_ID, "test", &path), id);
        export.entries.insert(
            id,
            ObjectRecord {
                scope: ROOT_ID,
                mount_name: "test".to_string(),
                path,
                parent: ROOT_ID,
                kind: NodeKind::File,
                size,
                size_exact: true,
                attrs: None,
                backing_path: Some(backing),
                last_seen: Instant::now(),
            },
        );
    }

    #[test]
    fn open_state_allows_non_exact_full_mode_to_reach_provider() {
        // Static-shape file routes can enter NFS with Unknown/Full
        // placeholder attrs before the file handler projects exact metadata.
        // They must reach provider materialization instead of being rejected
        // by the pre-read budget check. This empty test registry has no
        // runtime, so success for this guard is the later NoEnt path rather
        // than Resource.
        for size in [view_types::FileSize::Unknown, view_types::FileSize::NonZero] {
            let harness = empty_export();
            insert_full_mode_leaf(&harness.export, 700, "unknown-full", size);
            let result = harness.export.open_state(7, 700, 1, 1);
            assert!(
                matches!(result, Err(Status::NoEnt)),
                "expected provider lookup for full-mode {size:?}, got {result:?}"
            );
            assert!(harness.export.opens.active_inodes().is_empty());
        }
    }

    #[test]
    fn open_state_rejects_oversized_full_mode_declared_exact() {
        let harness = empty_export();
        insert_full_mode_leaf(
            &harness.export,
            500,
            "huge-full",
            view_types::FileSize::Exact(OPEN_MATERIALIZE_MAX_BYTES + 1),
        );

        let result = harness.export.open_state(
            7,
            500,
            harness.export.opens.active_inodes().len() as u64 + 1,
            1,
        );
        assert!(
            matches!(result, Err(Status::Resource)),
            "expected Resource for oversized full-mode OPEN, got {result:?}"
        );
        // The entry must not have been read from the (absent) provider runtime;
        // open-state table must remain empty.
        assert!(
            harness.export.opens.active_inodes().is_empty(),
            "OPEN must not register an open instance when it rejects the materialize"
        );
    }

    #[test]
    fn open_state_streams_oversized_backing_file() {
        let harness = empty_export();
        let temp = tempfile::tempdir().expect("backing tempdir");
        let backing = temp.path().join("huge.bin");
        // Use std::fs::File and set_len to create a sparse file larger than
        // the cap without consuming actual disk bytes.
        let file = std::fs::File::create(&backing).expect("create backing file");
        file.set_len(OPEN_MATERIALIZE_MAX_BYTES + 1)
            .expect("set backing len");
        drop(file);

        insert_backing_file(
            &harness.export,
            600,
            "huge-backing",
            backing,
            OPEN_MATERIALIZE_MAX_BYTES + 1,
        );

        let opened = harness
            .export
            .open_state(7, 600, 1, 1)
            .expect("backing open");
        let chunk = harness
            .export
            .read_state(opened.stateid, OPEN_MATERIALIZE_MAX_BYTES, 8)
            .expect("backing read");
        assert_eq!(chunk.data, vec![0]);
        assert!(chunk.eof);
    }

    #[test]
    fn idle_leaf_sweep_removes_entry_and_path_index() {
        let harness = empty_export();
        insert_stale_leaf(&harness.export, 100);

        harness.export.sweep_idle_entries();

        assert!(!harness.export.entries.contains_key(&100));
        assert!(!harness.export.path_to_inode.contains_key(&ObjectKey::new(
            ROOT_ID,
            "test",
            &test_path("stale-100")
        )));
    }

    #[test]
    fn idle_leaf_sweep_preserves_open_entries() {
        let harness = empty_export();
        insert_stale_leaf(&harness.export, 100);
        let _stateid = harness.export.opens.open(OpenSeed {
            generation: 1,
            inode: 100,
            clientid: 1,
            access: 1,
            materialized_bytes: Vec::new(),
        });

        harness.export.sweep_idle_entries();

        assert!(harness.export.entries.contains_key(&100));
        assert!(harness.export.path_to_inode.contains_key(&ObjectKey::new(
            ROOT_ID,
            "test",
            &test_path("stale-100")
        )));
    }

    #[test]
    fn learned_exact_size_survives_only_same_version_non_exact_refresh() {
        for (existing_version, incoming_size, incoming_version, expected) in [
            (
                Some("v1"),
                view_types::FileSize::Unknown,
                Some("v1"),
                Some(42),
            ),
            (
                Some("v1"),
                view_types::FileSize::Exact(7),
                Some("v1"),
                Some(7),
            ),
            (
                Some("current"),
                view_types::FileSize::Unknown,
                Some("next"),
                None,
            ),
            (None, view_types::FileSize::Unknown, None, None),
        ] {
            let existing = attrs(
                view_types::FileSize::Exact(42),
                view_types::Stability::Dynamic,
                existing_version,
            );
            let incoming = attrs(
                incoming_size,
                view_types::Stability::Dynamic,
                incoming_version,
            );

            let merged = frontend::merge_file_attrs(Some(&existing), Some(incoming)).unwrap();
            assert_eq!(
                exact_size(&merged),
                expected,
                "existing_version={existing_version:?}, incoming_size={incoming_size:?}, incoming_version={incoming_version:?}"
            );
        }
    }
}
