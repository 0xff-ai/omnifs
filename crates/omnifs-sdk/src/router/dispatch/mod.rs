//! `lookup_child`, `list_children`, `read_file`, and `open_file` dispatch.
//!
//! Each entry point resolves an absolute path against the sealed route
//! tables through a shared `Shape` view (`route_shape`), with the
//! literal-prefix auto-navigation machinery in `static_shape`. Common rules
//! across all entry points:
//!
//! - Candidate selection is per route kind, highest precedence first, with
//!   capture validators filtering candidacy (a typed-key parse rejection
//!   falls through to the next-most-specific route, not to not-found).
//! - Treeref routes win before anything else; below a handed-off subtree the
//!   host never calls the provider again.
//! - Literal prefixes of registered routes resolve and list as directories
//!   with no handler involved; listings merge handler enumerations with
//!   those static siblings and are non-exhaustive whenever a capture
//!   sibling exists at the next depth.

mod list;
mod lookup;
mod read;
mod route_shape;
mod static_shape;
