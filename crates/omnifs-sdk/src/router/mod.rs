//! The declarative provider router (ADR-0001 §10).
//!
//! A provider's `start` registers its whole path surface imperatively on a
//! [`Router`]; afterwards the `#[omnifs_sdk::provider]` macro glue calls
//! [`Router::seal`] (overlapping leaf claims fail initialization) and drives
//! every host browse call (`lookup_child`, `list_children`, `read_file`,
//! `open_file`) through the table. There are no per-route attribute macros.
//!
//! Registration verbs:
//!
//! - [`Router::dir`]: a directory route; the handler returns a
//!   [`crate::projection::DirProjection`].
//! - [`Router::file`]: a file route; the handler returns a
//!   [`crate::projection::FileProjection`].
//! - [`Router::treeref`]: a subtree handoff; the handler returns a
//!   [`crate::handler::TreeRef`] the host resolves to a bind-mounted tree.
//! - [`Router::object`] / [`Router::attach`]: bind a typed
//!   [`crate::object::Object`] whose [`crate::object::Key`] both loads and
//!   identifies the canonical resource; representations and field leaves are
//!   declared in a [`ObjectBlock`].
//!
//! Route templates are absolute paths built from literal segments,
//! `{capture}` segments, prefix captures (`@{resolver}`, `v{version}`), and
//! an optional trailing `{*rest}` multi-segment capture. The `pattern`
//! submodule owns the exact grammar and the precedence order dispatch uses.
//!
//! Contracts a route author can rely on:
//!
//! - Every literal prefix of a registered route is auto-navigable. Never
//!   write a stub handler just so `/a` exists on the way to `/a/{b}/c`.
//! - A capture parse rejection removes that route from candidacy; dispatch
//!   falls through to the next-most-specific matching route, not to
//!   not-found.
//! - `lookup_child` is the authoritative name oracle; `list_children` may be
//!   non-exhaustive, and a name absent from the latest listing can still
//!   resolve through lookup.
//! - Listings merge the handler's enumeration with the literal sibling
//!   routes registered at that depth, and report `exhaustive = false`
//!   whenever a capture sibling exists at the next depth.

mod dispatch;
mod handlers;
mod object;
pub(crate) mod pattern;
mod projection;
mod register;

#[cfg(test)]
mod tests;

pub use handlers::{
    IntoDirHandler, IntoFileHandler, IntoTreeRefHandler, NoCaptures, WithCaptures, WithKeyMethod,
    WithSyncKeyMethod,
};
pub use object::{ChildTopology, DirFace, FileFace, ObjectBlock, ObjectHandle, object};
pub(crate) use object::{EntryView, ResolvedChildView};
pub use register::{DirRoute, FileRoute, Router, TreeRefRoute};

/// The block builder for a file-shaped object anchor
/// ([`Router::file_object`]). The anchor projects as a single file (one
/// canonical/representation/direct/blob face), not a directory; the builder
/// surface is the same as [`ObjectBlock`].
pub type FileObjectBlock<O> = ObjectBlock<O>;
