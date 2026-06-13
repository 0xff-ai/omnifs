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

/// A host-synthesized entry that no provider projects, represented identically
/// for every renderer so FUSE and NFS materialize it the same way. The two
/// concrete cases differ in HOW their bytes are produced, captured by the
/// `content`:
///
/// - the mount-root ignore files (`.gitignore`/`.ignore`/`.rgignore`) are
///   `Fixed` static bytes (`@*\n`), so a recursive ignore-respecting tool skips
///   the `@`-prefixed control files during a tree walk;
/// - the pagination controls (`@next`/`@all`) are a `PaginationControl` ACTION:
///   reading one runs the host's accumulating pagination (advancing the parent
///   directory's cached dirents) and returns a one-line status, so the content
///   is computed at read time, not stored.
///
/// `Tree::resolve` returns a `Node` carrying this when the name is synthetic;
/// `Tree::list` appends `Entry`s carrying it; `Tree::read` dispatches on it.
/// A renderer never inspects the variant: it reads the node's bytes through
/// `Tree::read` and gets the right behavior for free. The renderer DOES learn
/// the leaf is synthetic (via `Node::synthetic`) so it can, e.g., serve the
/// bytes from a per-handle buffer and never re-run a mutating control action on
/// a partial read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Synthetic {
    pub content: SyntheticContent,
}

/// How a synthetic entry's bytes are produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyntheticContent {
    /// Static bytes served verbatim (the mount-root ignore files).
    Fixed(Vec<u8>),
    /// A pagination control action over the parent directory. `Next` advances
    /// one page; `All` advances to exhaustion (host-capped). The action mutates
    /// the parent's cached dirents and is resolved at read time.
    PaginationControl(PaginationControl),
}

/// Which pagination action a `@next`/`@all` control runs when read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaginationControl {
    Next,
    All,
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
    /// `Some` when this node is a host-synthesized entry (a pagination control
    /// or a mount-root ignore file) the renderer must read through `Tree::read`
    /// rather than dispatch to the provider.
    synthetic: Option<Synthetic>,
}

impl Node {
    pub fn new(mount: String, path: Path, meta: EntryMeta, backing: Backing) -> Self {
        Self {
            mount,
            path,
            meta,
            backing,
            synthetic: None,
        }
    }

    /// Construct a host-synthesized node (a pagination control or a mount-root
    /// ignore file). The `meta` carries the entry's projected attrs (so a
    /// renderer can stat it without a read); `synthetic` carries the byte source.
    pub fn synthetic(mount: String, path: Path, meta: EntryMeta, synthetic: Synthetic) -> Self {
        Self {
            mount,
            path,
            meta,
            backing: Backing::Provider,
            synthetic: Some(synthetic),
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

    /// The synthetic descriptor when this node is a host-synthesized entry; a
    /// renderer reads it through `Tree::read` and (for a control) must serve the
    /// result from a per-handle buffer so a partial read never re-runs the
    /// mutating action.
    pub fn synthetic_kind(&self) -> Option<&Synthetic> {
        self.synthetic.as_ref()
    }

    pub fn is_synthetic(&self) -> bool {
        self.synthetic.is_some()
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
/// and reads attrs from meta without a second resolve. `synthetic` is `Some`
/// for the host-synthesized control / ignore entries `Tree` appends to a
/// listing; the renderer marks its inode/handle so a later `read` of that child
/// goes back through `Tree::read` (where the synthetic byte source is served)
/// rather than the provider.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub meta: EntryMeta,
    pub synthetic: Option<Synthetic>,
}

impl Entry {
    /// A normal provider-projected child.
    pub fn provider(name: String, meta: EntryMeta) -> Self {
        Self {
            name,
            meta,
            synthetic: None,
        }
    }
}
