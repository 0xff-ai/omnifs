//! Renderer-neutral node and identity types.

use std::path::PathBuf;

use crate::tree_refs::TreeRef;
use crate::view::{EntryMeta, FileAttrsCache};
use omnifs_core::path::Path;

/// Where a node's bytes/children live. `Provider` is the normal projected
/// case; `Host` is a Git tree rooted at a retained capability plus a relative
/// path; `Synthetic` is
/// host-produced content that no provider projected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeBody {
    Provider,
    Host {
        tree_ref: TreeRef,
        relative: PathBuf,
        kind: HostKind,
    },
    Synthetic(Synthetic),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostKind {
    Directory,
    File,
    Symlink,
}

/// A host-synthesized entry that no provider projects, represented identically
/// for every renderer so FUSE and NFS materialize it the same way. The two
/// concrete cases differ in HOW their bytes are produced, captured by the
/// `content`:
///
/// - the mount-root ignore files (`.gitignore`/`.ignore`/`.rgignore`) are
///   `Fixed` static bytes, so a recursive ignore-respecting tool skips the
///   `@`-prefixed control files and generated README leaves during a tree walk;
/// - the pagination controls (`@next`/`@all`) are a `PaginationControl` ACTION:
///   reading one runs the host's accumulating pagination (advancing the parent
///   directory's cached dirents) and returns a one-line status, so the content
///   is computed at read time, not stored.
///
/// Internal resolution returns a `Node` carrying this when the name is synthetic;
/// provider listing appends `Entry`s carrying it; internal reads dispatch on it.
/// A renderer never inspects the variant: it reads the node's bytes through
/// the internal read path and gets the right behavior for free. The renderer DOES learn
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

/// Resolved tree node. Carries the renderer-neutral identity + `EntryMeta` a
/// renderer turns into its own kernel/protocol identity (a FUSE inode + FileAttr,
/// an NFS filehandle + fattr4). Carries NO inode number, NO filehandle, NO fuser
/// FileAttr. `EntryMeta` is the substrate's own type (kind + Option<FileAttrsCache>),
/// reused flat rather than re-encoded into an enum, so EffectApplier/cache/
/// LookupOutcome speak the same shape at every boundary.
#[derive(Debug, Clone)]
pub struct Node {
    mount: String,
    path: Path,
    meta: EntryMeta,
    body: NodeBody,
}

impl Node {
    pub fn new(mount: String, path: Path, meta: EntryMeta, body: NodeBody) -> Self {
        Self {
            mount,
            path,
            meta,
            body,
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
            body: NodeBody::Synthetic(synthetic),
        }
    }

    pub fn mount(&self) -> &str {
        &self.mount
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn attrs(&self) -> Option<&FileAttrsCache> {
        self.meta.attrs()
    }

    pub fn is_dir(&self) -> bool {
        match self.host() {
            Some((_, _, HostKind::Directory)) => true,
            Some((_, _, HostKind::File | HostKind::Symlink)) => false,
            None => self.meta.is_directory(),
        }
    }

    pub(crate) fn host(&self) -> Option<(&TreeRef, &PathBuf, HostKind)> {
        match &self.body {
            NodeBody::Host {
                tree_ref,
                relative,
                kind,
            } => Some((tree_ref, relative, *kind)),
            NodeBody::Provider | NodeBody::Synthetic(_) => None,
        }
    }

    /// The synthetic descriptor when this node is a host-synthesized entry; a
    /// renderer reads it through the namespace and (for a control) must serve the
    /// result from a per-handle buffer so a partial read never re-runs the
    /// mutating action.
    pub(crate) fn synthetic_kind(&self) -> Option<&Synthetic> {
        match &self.body {
            NodeBody::Synthetic(synthetic) => Some(synthetic),
            NodeBody::Provider | NodeBody::Host { .. } => None,
        }
    }
}

/// One child within a `Listing`. Renderer-neutral: name + meta + origin. The
/// renderer mints its own inode/filehandle over (parent.mount, parent.path.join(name))
/// and reads attrs from meta without a second resolve. Synthetic entries carry
/// their byte source directly, so a frontend can mark its inode/handle without
/// side-channel vectors.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub meta: EntryMeta,
    pub origin: EntryOrigin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryOrigin {
    Provider,
    Synthetic(Synthetic),
}

impl Entry {
    /// A normal provider-projected child.
    pub(crate) fn provider(name: String, meta: EntryMeta) -> Self {
        Self {
            name,
            meta,
            origin: EntryOrigin::Provider,
        }
    }

    /// A host-synthesized child surfaced by `Tree` and materialized by a
    /// frontend through the namespace.
    pub(crate) fn synthetic(name: String, meta: EntryMeta, synthetic: Synthetic) -> Self {
        Self {
            name,
            meta,
            origin: EntryOrigin::Synthetic(synthetic),
        }
    }
}
