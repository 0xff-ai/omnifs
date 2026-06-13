//! Renderer-neutral node and identity types.

use std::path::PathBuf;

use omnifs_core::path::Path;
use omnifs_core::view::{EntryKind, EntryMeta, FileAttrsCache};

/// Where a node's bytes/children live. `Provider` is the normal projected
/// case; `Subtree` is a treeref already resolved (via `Runtime::resolve_tree_ref`)
/// to a bind-mounted clone/archive dir, captured at resolve time so read/list
/// branch to passthrough without a second provider round trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backing {
    Provider,
    Subtree(PathBuf),
}

/// Stable, content-addressable identity = (mount, mount-relative protocol path).
/// This IS the cache key everywhere in omnifs, so it survives Tree-internal
/// eviction: a renderer encodes it (or a hash) into a FUSE inode-table key or a
/// 16-byte NFS filehandle and rehydrates cheaply via `Tree::resolve` after
/// eviction, without re-walking from root. This answers the NFSv4
/// filehandle-first (PUTFH hands a bare handle) requirement.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NodeId {
    pub mount: String,
    pub path: Path,
}

/// Resolved tree node. Carries the renderer-neutral identity + `EntryMeta` a
/// renderer turns into its own kernel/protocol identity (a FUSE inode + FileAttr,
/// an NFS filehandle + fattr4). Carries NO inode number, NO filehandle, NO fuser
/// FileAttr. `EntryMeta` is the substrate's own type (kind + Option<FileAttrsCache>),
/// reused flat rather than re-encoded into an enum, so Materializer/cache/
/// LookupOutcome speak the same shape at every boundary.
#[derive(Debug, Clone)]
pub struct Node {
    mount: String,
    path: Path,
    meta: EntryMeta,
    backing: Backing,
}

impl Node {
    pub fn new(mount: String, path: Path, meta: EntryMeta, backing: Backing) -> Self {
        Self {
            mount,
            path,
            meta,
            backing,
        }
    }

    pub fn mount(&self) -> &str {
        &self.mount
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn meta(&self) -> &EntryMeta {
        &self.meta
    }

    pub fn kind(&self) -> EntryKind {
        self.meta.kind
    }

    pub fn attrs(&self) -> Option<&FileAttrsCache> {
        self.meta.attrs.as_ref()
    }

    pub fn st_size(&self) -> u64 {
        self.meta.st_size()
    }

    pub fn is_dir(&self) -> bool {
        self.meta.is_directory()
    }

    pub fn is_file(&self) -> bool {
        self.meta.is_file()
    }

    pub fn backing(&self) -> &Backing {
        &self.backing
    }

    pub fn is_backing(&self) -> bool {
        !matches!(self.backing, Backing::Provider)
    }

    /// Stable identity the renderer persists in its kernel handle.
    pub fn id(&self) -> NodeId {
        NodeId {
            mount: self.mount.clone(),
            path: self.path.clone(),
        }
    }
}

/// One child within a `Listing`. Renderer-neutral: name + meta. The renderer
/// mints its own inode/filehandle over (parent.mount, parent.path.join(name))
/// and reads attrs from meta without a second resolve.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub meta: EntryMeta,
}
