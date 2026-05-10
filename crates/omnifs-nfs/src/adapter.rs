use crate::export::{NfsAttr, NfsDirEntry, NfsNodeKind, NfsResult, ReadOnlyExport};
use crate::protocol::consts::{
    DEFAULT_EXPORT_NAME, EXPORT_ALIAS_ID, NFS4ERR_ACCESS, NFS4ERR_INVAL, NFS4ERR_IO, NFS4ERR_ISDIR,
    NFS4ERR_NOENT, NFS4ERR_NOTDIR, NFS4ERR_RESOURCE, NFS4ERR_STALE, ROOT_ID,
};
use crate::protocol::filehandle::now_sec;
use crate::protocol::name::is_valid_component;
use dashmap::DashMap;
use omnifs_host::cache::{self, CacheRecord, RecordKind};
use omnifs_host::omnifs::provider::types::{
    EntryKind, ErrorKind, ListResult, LookupResult, OpResult, ProviderError,
};
use omnifs_host::path_key::{PathKey, PathToInode};
use omnifs_host::registry::ProviderRegistry;
use omnifs_host::runtime::CalloutRuntime;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;
use tokio::runtime::Handle;

#[derive(Debug, Clone)]
struct NfsEntry {
    mount_name: String,
    path: String,
    parent: u64,
    kind: NfsNodeKind,
    size: u64,
    size_exact: bool,
    backing_path: Option<PathBuf>,
}

pub struct OmnifsExport {
    rt: Handle,
    registry: Arc<ProviderRegistry>,
    entries: DashMap<u64, NfsEntry>,
    path_to_inode: Arc<PathToInode>,
    next_ino: AtomicU64,
    root_mount: Option<String>,
}

impl OmnifsExport {
    pub fn new(rt: Handle, registry: Arc<ProviderRegistry>) -> Self {
        let root_mount = registry.root_mount_name().map(str::to_string);
        let entries = DashMap::new();
        let path_to_inode = Arc::new(DashMap::new());
        let root_entry = NfsEntry {
            mount_name: root_mount.clone().unwrap_or_default(),
            path: String::new(),
            parent: ROOT_ID,
            kind: NfsNodeKind::Directory,
            size: 0,
            size_exact: true,
            backing_path: None,
        };
        if let Some(mount) = &root_mount {
            path_to_inode.insert(PathKey::new(mount, ""), ROOT_ID);
        }
        let export_alias_entry = NfsEntry {
            mount_name: root_mount.clone().unwrap_or_default(),
            path: String::new(),
            parent: ROOT_ID,
            kind: NfsNodeKind::Directory,
            size: 0,
            size_exact: true,
            backing_path: None,
        };
        entries.insert(ROOT_ID, root_entry);
        entries.insert(EXPORT_ALIAS_ID, export_alias_entry);
        Self {
            rt,
            registry,
            entries,
            path_to_inode,
            next_ino: AtomicU64::new(EXPORT_ALIAS_ID + 1),
            root_mount,
        }
    }

    fn runtime_for_mount(&self, mount: &str) -> Option<Arc<CalloutRuntime>> {
        self.registry.get(mount).cloned()
    }

    fn alloc_ino(&self) -> u64 {
        self.next_ino.fetch_add(1, Ordering::Relaxed)
    }

    #[allow(clippy::too_many_arguments)]
    fn get_or_alloc(
        &self,
        mount_name: &str,
        path: &str,
        parent: u64,
        kind: NfsNodeKind,
        size: u64,
        size_exact: bool,
        backing_path: Option<PathBuf>,
    ) -> u64 {
        let key = PathKey::new(mount_name, path);
        *self
            .path_to_inode
            .entry(key)
            .and_modify(|existing| {
                if let Some(mut entry) = self.entries.get_mut(existing) {
                    entry.parent = parent;
                    entry.kind = kind;
                    if size_exact || !entry.size_exact {
                        entry.size = size;
                        entry.size_exact = size_exact;
                    }
                    if backing_path.is_some() {
                        entry.backing_path.clone_from(&backing_path);
                        entry.size_exact = true;
                    }
                }
            })
            .or_insert_with(|| {
                let id = self.alloc_ino();
                self.entries.insert(
                    id,
                    NfsEntry {
                        mount_name: mount_name.to_string(),
                        path: path.to_string(),
                        parent,
                        kind,
                        size,
                        size_exact,
                        backing_path,
                    },
                );
                id
            })
    }

    fn update_file_size_after_read(&self, id: u64, size: u64) {
        if let Some(mut entry) = self.entries.get_mut(&id)
            && entry.kind == NfsNodeKind::File
            && entry.backing_path.is_none()
        {
            entry.size = size;
            entry.size_exact = true;
        }
    }

    fn cache_file_metadata(runtime: &CalloutRuntime, path: &str, attrs: cache::FileAttrsCache) {
        let meta = cache::EntryMeta::file(attrs);
        let lookup = cache::LookupPayload::Positive(meta.clone());
        if let Some(payload) = lookup.serialize() {
            runtime.cache_put(
                path,
                RecordKind::Lookup,
                &CacheRecord::new(RecordKind::Lookup, payload),
            );
        }

        let attr = cache::AttrPayload { meta };
        if let Some(payload) = attr.serialize() {
            runtime.cache_put(
                path,
                RecordKind::Attr,
                &CacheRecord::new(RecordKind::Attr, payload),
            );
        }
    }

    fn cache_file_content(
        runtime: &CalloutRuntime,
        path: &str,
        attrs: &cache::FileAttrsCache,
        data: &[u8],
    ) {
        let Some(aux) = attrs.durable_cache_aux() else {
            return;
        };
        let payload = cache::FilePayload::new(attrs.version_token.clone(), data.to_vec());
        let Some(payload) = payload.serialize() else {
            return;
        };
        runtime.cache_put_with_aux(
            path,
            RecordKind::File,
            aux.as_deref(),
            &CacheRecord::new(RecordKind::File, payload),
        );
    }

    fn attr_from_entry(id: u64, entry: &NfsEntry) -> NfsAttr {
        NfsAttr {
            id,
            parent: entry.parent,
            kind: entry.kind,
            size: entry.size,
            mode: match entry.kind {
                NfsNodeKind::Directory => 0o555,
                NfsNodeKind::File => 0o444,
                NfsNodeKind::Symlink => 0o777,
            },
            change: id.wrapping_mul(1_000_003).wrapping_add(entry.size),
            mtime_sec: now_sec(),
        }
    }

    fn attr_from_metadata(id: u64, parent: u64, metadata: &std::fs::Metadata) -> NfsAttr {
        let kind = if metadata.is_dir() {
            NfsNodeKind::Directory
        } else if metadata.file_type().is_symlink() {
            NfsNodeKind::Symlink
        } else {
            NfsNodeKind::File
        };
        let mtime_sec = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or_else(now_sec, |duration| {
                i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
            });
        NfsAttr {
            id,
            parent,
            kind,
            size: metadata.len(),
            mode: match kind {
                NfsNodeKind::Directory => 0o555,
                NfsNodeKind::File => 0o444,
                NfsNodeKind::Symlink => 0o777,
            },
            change: id.wrapping_mul(1_000_003).wrapping_add(metadata.len()),
            mtime_sec,
        }
    }

    fn entry_kind(kind: &EntryKind) -> NfsNodeKind {
        match kind {
            EntryKind::Directory => NfsNodeKind::Directory,
            EntryKind::File(_) => NfsNodeKind::File,
        }
    }

    fn cached_entry_kind(kind: cache::EntryKindCache) -> NfsNodeKind {
        match kind {
            cache::EntryKindCache::Directory => NfsNodeKind::Directory,
            cache::EntryKindCache::File => NfsNodeKind::File,
        }
    }

    fn cached_entry_exact(meta: &cache::EntryMeta) -> bool {
        meta.attrs
            .as_ref()
            .is_some_and(|attrs| matches!(attrs.size, cache::SizeCache::Exact(_)))
    }

    fn attrs_after_full_read(
        attrs: cache::FileAttrsCache,
        content_len: usize,
    ) -> cache::FileAttrsCache {
        match attrs.size {
            cache::SizeCache::Exact(_) => attrs,
            cache::SizeCache::NonZero | cache::SizeCache::Unknown => {
                attrs.with_exact_size(u64::try_from(content_len).unwrap_or(u64::MAX))
            },
        }
    }

    fn read_result_payload(
        runtime: &CalloutRuntime,
        path: &str,
        result: omnifs_host::omnifs::provider::types::FileContentResult,
    ) -> NfsResult<(Vec<u8>, cache::FileAttrsCache)> {
        match result {
            omnifs_host::omnifs::provider::types::FileContentResult::Inline(inline) => {
                Ok((inline.content, cache::FileAttrsCache::from(&inline.attrs)))
            },
            omnifs_host::omnifs::provider::types::FileContentResult::Blob(blob) => {
                let data = runtime.read_blob_full(blob.blob).map_err(|error| {
                    tracing::warn!(
                        op = "read",
                        path = %path,
                        error = %error,
                        "NFS blob-backed read failed"
                    );
                    NFS4ERR_IO
                })?;
                Ok((data, cache::FileAttrsCache::from(&blob.attrs)))
            },
        }
    }

    fn provider_status(error: &ProviderError) -> u32 {
        match error.kind {
            ErrorKind::NotFound => NFS4ERR_NOENT,
            ErrorKind::NotADirectory => NFS4ERR_NOTDIR,
            ErrorKind::NotAFile => NFS4ERR_ISDIR,
            ErrorKind::PermissionDenied | ErrorKind::Denied => NFS4ERR_ACCESS,
            ErrorKind::InvalidInput => NFS4ERR_INVAL,
            _ => NFS4ERR_IO,
        }
    }

    fn op_result_name(result: &OpResult) -> &'static str {
        match result {
            OpResult::List(_) => "list",
            OpResult::Lookup(_) => "lookup",
            OpResult::Read(_) => "read",
            OpResult::Err(_) => "err",
            OpResult::Event(_) => "event",
            _ => "other",
        }
    }

    fn child_path(parent_path: &str, name: &str) -> String {
        if parent_path.is_empty() {
            name.to_string()
        } else {
            format!("{parent_path}/{name}")
        }
    }

    fn lookup_from_cached_dirents(
        &self,
        mount_name: &str,
        parent_path: &str,
        parent: u64,
        name: &str,
        runtime: &CalloutRuntime,
    ) -> Option<NfsResult<u64>> {
        let record = runtime.cache_get(parent_path, RecordKind::Dirents)?;
        let dirents = cache::DirentsPayload::deserialize(&record.payload)?;
        if !dirents.exhaustive {
            return None;
        }
        let Some(dirent) = dirents.entries.iter().find(|entry| entry.name == name) else {
            return Some(Err(NFS4ERR_NOENT));
        };
        let path = Self::child_path(parent_path, name);
        let id = self.get_or_alloc(
            mount_name,
            &path,
            parent,
            Self::cached_entry_kind(dirent.meta.kind),
            dirent.meta.st_size(),
            Self::cached_entry_exact(&dirent.meta),
            None,
        );
        Some(Ok(id))
    }

    fn lookup_from_cached_lookup(
        &self,
        mount_name: &str,
        child_path: &str,
        parent: u64,
        name: &str,
        runtime: &CalloutRuntime,
    ) -> Option<NfsResult<u64>> {
        let record = runtime.cache_get(child_path, RecordKind::Lookup)?;
        let lookup = cache::LookupPayload::deserialize(&record.payload)?;
        match lookup {
            cache::LookupPayload::Negative => Some(Err(NFS4ERR_NOENT)),
            cache::LookupPayload::Positive(meta) => {
                let id = self.get_or_alloc(
                    mount_name,
                    child_path,
                    parent,
                    Self::cached_entry_kind(meta.kind),
                    meta.st_size(),
                    Self::cached_entry_exact(&meta),
                    None,
                );
                let _ = name;
                Some(Ok(id))
            },
        }
    }

    fn lookup_via_provider(
        &self,
        mount_name: &str,
        parent_path: &str,
        parent: u64,
        name: &str,
        runtime: &Arc<CalloutRuntime>,
    ) -> NfsResult<u64> {
        let child_path = Self::child_path(parent_path, name);
        match self
            .rt
            .block_on(runtime.call_lookup_child(parent_path, name))
        {
            Ok(OpResult::Lookup(LookupResult::Subtree(tree_ref))) => {
                let Some(real_root) = runtime.resolve_tree_ref(tree_ref) else {
                    return Err(NFS4ERR_IO);
                };
                Ok(self.get_or_alloc(
                    mount_name,
                    &child_path,
                    parent,
                    NfsNodeKind::Directory,
                    0,
                    true,
                    Some(real_root),
                ))
            },
            Ok(OpResult::Lookup(LookupResult::Entry(entry))) => {
                let meta = cache::EntryMeta::from(&entry.target.kind);
                let size = meta.st_size();
                let size_exact = Self::cached_entry_exact(&meta);
                let kind = Self::entry_kind(&entry.target.kind);
                let id = self.get_or_alloc(
                    mount_name,
                    &child_path,
                    parent,
                    kind,
                    size,
                    size_exact,
                    None,
                );
                let payload = cache::LookupPayload::Positive(meta);
                if let Some(encoded) = payload.serialize() {
                    runtime.cache_put(
                        &child_path,
                        RecordKind::Lookup,
                        &CacheRecord::new(RecordKind::Lookup, encoded),
                    );
                }
                Ok(id)
            },
            Ok(OpResult::Lookup(LookupResult::NotFound)) => {
                let payload = cache::LookupPayload::Negative;
                if let Some(encoded) = payload.serialize() {
                    runtime.cache_put(
                        &child_path,
                        RecordKind::Lookup,
                        &CacheRecord::new(RecordKind::Lookup, encoded),
                    );
                }
                Err(NFS4ERR_NOENT)
            },
            Ok(OpResult::Err(error)) => {
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
                Err(Self::provider_status(&error))
            },
            Ok(result) => {
                tracing::warn!(
                    op = "lookup",
                    mount = %mount_name,
                    parent = %parent_path,
                    name = %name,
                    result = Self::op_result_name(&result),
                    "NFS provider lookup returned unexpected result"
                );
                Err(NFS4ERR_IO)
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
                Err(NFS4ERR_IO)
            },
        }
    }

    fn readdir_from_dirents(
        &self,
        mount_name: &str,
        path: &str,
        parent: u64,
        dirents: &cache::DirentsPayload,
    ) -> Vec<NfsDirEntry> {
        dirents
            .entries
            .iter()
            .map(|entry| {
                let child_path = Self::child_path(path, &entry.name);
                let kind = Self::cached_entry_kind(entry.meta.kind);
                let id = self.get_or_alloc(
                    mount_name,
                    &child_path,
                    parent,
                    kind,
                    entry.meta.st_size(),
                    Self::cached_entry_exact(&entry.meta),
                    None,
                );
                let attr = self.attr(id).unwrap_or_else(|_| NfsAttr {
                    id,
                    parent,
                    kind,
                    size: entry.meta.st_size(),
                    mode: 0o444,
                    change: id,
                    mtime_sec: now_sec(),
                });
                NfsDirEntry {
                    id,
                    name: entry.name.clone(),
                    attr,
                }
            })
            .collect()
    }

    fn readdir_backing(
        &self,
        mount_name: &str,
        path: &str,
        parent: u64,
        root: &Path,
    ) -> NfsResult<Vec<NfsDirEntry>> {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(root).map_err(|_| NFS4ERR_IO)? {
            let entry = entry.map_err(|_| NFS4ERR_IO)?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let child_path = Self::child_path(path, name);
            let backing_path = entry.path();
            let metadata = std::fs::symlink_metadata(&backing_path).map_err(|_| NFS4ERR_IO)?;
            let kind = if metadata.is_dir() {
                NfsNodeKind::Directory
            } else if metadata.file_type().is_symlink() {
                NfsNodeKind::Symlink
            } else {
                NfsNodeKind::File
            };
            let id = self.get_or_alloc(
                mount_name,
                &child_path,
                parent,
                kind,
                metadata.len(),
                true,
                Some(backing_path),
            );
            entries.push(NfsDirEntry {
                id,
                name: name.to_string(),
                attr: Self::attr_from_metadata(id, parent, &metadata),
            });
        }
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(entries)
    }

    fn readdir_via_provider(
        &self,
        mount_name: &str,
        path: &str,
        parent: u64,
        runtime: &Arc<CalloutRuntime>,
    ) -> NfsResult<Vec<NfsDirEntry>> {
        match self.rt.block_on(runtime.call_list_children(path)) {
            Ok(OpResult::List(ListResult::Subtree(tree_ref))) => {
                let Some(real_root) = runtime.resolve_tree_ref(tree_ref) else {
                    return Err(NFS4ERR_IO);
                };
                if let Some(mut entry) = self.entries.get_mut(&parent) {
                    entry.backing_path = Some(real_root.clone());
                }
                self.readdir_backing(mount_name, path, parent, &real_root)
            },
            Ok(OpResult::List(ListResult::Entries(listing))) => {
                let mut records = Vec::with_capacity(listing.entries.len());
                for entry in &listing.entries {
                    records.push(cache::DirentRecord {
                        name: entry.name.clone(),
                        meta: cache::EntryMeta::from(&entry.kind),
                    });
                }
                let dirents = cache::DirentsPayload {
                    entries: records,
                    exhaustive: listing.exhaustive,
                };
                if let Some(encoded) = dirents.serialize() {
                    runtime.cache_put(
                        path,
                        RecordKind::Dirents,
                        &CacheRecord::new(RecordKind::Dirents, encoded),
                    );
                }
                Ok(self.readdir_from_dirents(mount_name, path, parent, &dirents))
            },
            Ok(OpResult::Err(error)) => {
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
            Ok(OpResult::Read(_)) => {
                tracing::warn!(
                    op = "readdir",
                    mount = %mount_name,
                    path = %path,
                    result = "read",
                    "NFS provider readdir resolved to a file"
                );
                Err(NFS4ERR_NOTDIR)
            },
            Ok(result) => {
                tracing::warn!(
                    op = "readdir",
                    mount = %mount_name,
                    path = %path,
                    result = Self::op_result_name(&result),
                    "NFS provider readdir returned unexpected result"
                );
                Err(NFS4ERR_IO)
            },
            Err(error) => {
                tracing::warn!(
                    op = "readdir",
                    mount = %mount_name,
                    path = %path,
                    error = %error,
                    "NFS runtime readdir failed"
                );
                Err(NFS4ERR_IO)
            },
        }
    }
}

impl ReadOnlyExport for OmnifsExport {
    fn root(&self) -> u64 {
        ROOT_ID
    }

    fn attr(&self, id: u64) -> NfsResult<NfsAttr> {
        let entry = self.entries.get(&id).ok_or(NFS4ERR_STALE)?;
        if let Some(path) = &entry.backing_path {
            let metadata = std::fs::symlink_metadata(path).map_err(|_| NFS4ERR_STALE)?;
            Ok(Self::attr_from_metadata(id, entry.parent, &metadata))
        } else {
            Ok(Self::attr_from_entry(id, &entry))
        }
    }

    fn lookup(&self, parent: u64, name: &str) -> NfsResult<u64> {
        if !is_valid_component(name) {
            return Err(NFS4ERR_INVAL);
        }

        if parent == ROOT_ID && name == DEFAULT_EXPORT_NAME {
            return Ok(EXPORT_ALIAS_ID);
        }

        if (parent == ROOT_ID || parent == EXPORT_ALIAS_ID)
            && self.root_mount.is_none()
            && self.registry.get(name).is_some()
        {
            return Ok(self.get_or_alloc(name, "", parent, NfsNodeKind::Directory, 0, true, None));
        }

        let parent_entry = self.entries.get(&parent).ok_or(NFS4ERR_STALE)?;
        if parent_entry.kind != NfsNodeKind::Directory {
            return Err(NFS4ERR_NOTDIR);
        }
        let mount_name = parent_entry.mount_name.clone();
        let parent_path = parent_entry.path.clone();
        let backing_path = parent_entry.backing_path.clone();
        drop(parent_entry);

        if let Some(root) = backing_path {
            let child = root.join(name);
            let metadata = std::fs::symlink_metadata(&child).map_err(|_| NFS4ERR_NOENT)?;
            let kind = if metadata.is_dir() {
                NfsNodeKind::Directory
            } else if metadata.file_type().is_symlink() {
                NfsNodeKind::Symlink
            } else {
                NfsNodeKind::File
            };
            let child_path = Self::child_path(&parent_path, name);
            return Ok(self.get_or_alloc(
                &mount_name,
                &child_path,
                parent,
                kind,
                metadata.len(),
                true,
                Some(child),
            ));
        }

        let runtime = self.runtime_for_mount(&mount_name).ok_or(NFS4ERR_NOENT)?;
        let child_path = Self::child_path(&parent_path, name);
        if let Some(result) =
            self.lookup_from_cached_dirents(&mount_name, &parent_path, parent, name, &runtime)
        {
            return result;
        }
        if let Some(result) =
            self.lookup_from_cached_lookup(&mount_name, &child_path, parent, name, &runtime)
        {
            return result;
        }
        self.lookup_via_provider(&mount_name, &parent_path, parent, name, &runtime)
    }

    fn readdir(&self, id: u64) -> NfsResult<Vec<NfsDirEntry>> {
        if (id == ROOT_ID || id == EXPORT_ALIAS_ID) && self.root_mount.is_none() {
            let mut mounts = self.registry.mounts();
            mounts.sort();
            return Ok(mounts
                .into_iter()
                .map(|mount| {
                    let child =
                        self.get_or_alloc(&mount, "", id, NfsNodeKind::Directory, 0, true, None);
                    NfsDirEntry {
                        id: child,
                        name: mount,
                        attr: self.attr(child).expect("fresh mount attr"),
                    }
                })
                .collect());
        }

        let entry = self.entries.get(&id).ok_or(NFS4ERR_STALE)?;
        if entry.kind != NfsNodeKind::Directory {
            return Err(NFS4ERR_NOTDIR);
        }
        let mount_name = entry.mount_name.clone();
        let path = entry.path.clone();
        let backing_path = entry.backing_path.clone();
        drop(entry);

        if let Some(root) = backing_path {
            return self.readdir_backing(&mount_name, &path, id, &root);
        }

        let runtime = self.runtime_for_mount(&mount_name).ok_or(NFS4ERR_NOENT)?;
        if let Some(record) = runtime.cache_get(&path, RecordKind::Dirents)
            && let Some(dirents) = cache::DirentsPayload::deserialize(&record.payload)
            && dirents.exhaustive
        {
            return Ok(self.readdir_from_dirents(&mount_name, &path, id, &dirents));
        }

        self.readdir_via_provider(&mount_name, &path, id, &runtime)
    }

    fn read(&self, id: u64) -> NfsResult<Vec<u8>> {
        let entry = self.entries.get(&id).ok_or(NFS4ERR_STALE)?;
        if entry.kind == NfsNodeKind::Directory {
            return Err(NFS4ERR_ISDIR);
        }
        if entry.kind == NfsNodeKind::Symlink {
            return Err(NFS4ERR_INVAL);
        }
        let mount_name = entry.mount_name.clone();
        let path = entry.path.clone();
        let backing_path = entry.backing_path.clone();
        drop(entry);

        if let Some(backing_path) = backing_path {
            let metadata = std::fs::symlink_metadata(&backing_path).map_err(|_| NFS4ERR_STALE)?;
            if metadata.file_type().is_symlink() {
                return Err(NFS4ERR_INVAL);
            }
            if metadata.is_dir() {
                return Err(NFS4ERR_ISDIR);
            }
            return std::fs::read(backing_path).map_err(|_| NFS4ERR_IO);
        }

        let runtime = self.runtime_for_mount(&mount_name).ok_or(NFS4ERR_NOENT)?;
        if let Some(record) = runtime.cache_get(&path, RecordKind::File)
            && let Some(payload) = cache::FilePayload::deserialize(&record.payload)
        {
            let data = payload.content;
            let size = u64::try_from(data.len()).unwrap_or(u64::MAX);
            self.update_file_size_after_read(id, size);
            return Ok(data);
        }

        match self.rt.block_on(runtime.call_read_file(&path)) {
            Ok(OpResult::Read(result)) => {
                let (data, attrs) = Self::read_result_payload(&runtime, &path, result)?;
                let attrs = Self::attrs_after_full_read(attrs, data.len());
                let size = u64::try_from(data.len()).unwrap_or(u64::MAX);
                Self::cache_file_content(&runtime, &path, &attrs, &data);
                self.update_file_size_after_read(id, size);
                Self::cache_file_metadata(&runtime, &path, attrs);
                Ok(data)
            },
            Ok(OpResult::Err(error)) => {
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
            Ok(result) => {
                tracing::warn!(
                    op = "read",
                    mount = %mount_name,
                    path = %path,
                    result = Self::op_result_name(&result),
                    "NFS provider read returned unexpected result"
                );
                Err(NFS4ERR_IO)
            },
            Err(error) => {
                tracing::warn!(
                    op = "read",
                    mount = %mount_name,
                    path = %path,
                    error = %error,
                    "NFS runtime read failed"
                );
                Err(NFS4ERR_IO)
            },
        }
    }

    fn readlink(&self, id: u64) -> NfsResult<Vec<u8>> {
        let entry = self.entries.get(&id).ok_or(NFS4ERR_STALE)?;
        if entry.kind != NfsNodeKind::Symlink {
            return Err(NFS4ERR_INVAL);
        }
        let Some(path) = entry.backing_path.clone() else {
            return Err(NFS4ERR_INVAL);
        };
        std::fs::read_link(path)
            .map(|target| target.as_os_str().as_encoded_bytes().to_vec())
            .map_err(|_| NFS4ERR_IO)
    }

    fn materialize_for_open(&self, id: u64, limit: usize) -> NfsResult<usize> {
        if let Some(entry) = self.entries.get(&id)
            && entry.size_exact
            && entry.size > limit as u64
        {
            return Err(NFS4ERR_RESOURCE);
        }
        let data = self.read(id)?;
        if data.len() > limit {
            return Err(NFS4ERR_RESOURCE);
        }
        Ok(data.len())
    }
}
