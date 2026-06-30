//! Logical object identity: what makes two paths "the same object".
//!
//! A [`LogicalId`] (object kind plus identity captures) is the key the
//! host caches canonical bytes under. Paths that should share one cached
//! object must derive the same id; captures that vary across route aliases
//! without changing the underlying object (a version selector, a category
//! filter) are wrapped in [`Facet`] so they stay out of identity.

use std::fmt;
use std::ops::Deref;

use crate::object::ObjectKind;
use omnifs_wit::provider::types as wit;

/// The identity-contributing captures of a key, in declaration order.
/// Emitted by `#[path_captures]`, which skips [`Facet`] fields.
pub trait IdentityCaptures {
    fn identity_captures(&self) -> Vec<(&'static str, String)>;
}

/// A provider-local logical identity: an object kind plus its identity captures
/// in declaration order.
///
/// Equality is order-sensitive: the capture sequence is part of the
/// identity, so reordering fields in a `#[path_captures]` struct changes
/// every id it produces (and orphans previously cached objects). The
/// `Display` form is `kind|name=value|...`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogicalId {
    pub kind: ObjectKind,
    pub captures: Vec<(&'static str, String)>,
}

impl LogicalId {
    pub fn new(kind: ObjectKind, captures: Vec<(&'static str, String)>) -> Self {
        Self { kind, captures }
    }
}

impl fmt::Display for LogicalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.kind.as_str())?;
        for (name, value) in &self.captures {
            write!(f, "|{name}={value}")?;
        }
        Ok(())
    }
}

impl From<LogicalId> for wit::LogicalId {
    fn from(id: LogicalId) -> Self {
        Self {
            kind: id.kind.as_str().to_string(),
            captures: id
                .captures
                .into_iter()
                .map(|(name, value)| wit::IdCapture {
                    name: name.to_string(),
                    value,
                })
                .collect(),
        }
    }
}

impl LogicalId {
    /// Structural equality against a host-pushed wire id, allocation-free. The
    /// wire id carries owned runtime strings while a `LogicalId`'s capture names
    /// are `&'static`, so a `From<wit::LogicalId>` would have to leak; the host
    /// pushes a wire id on every warm read for the self-check, so compare in
    /// place instead of reconstructing.
    pub fn matches_wire(&self, wire: &wit::LogicalId) -> bool {
        self.kind.as_str() == wire.kind.as_str()
            && self.captures.len() == wire.captures.len()
            && self
                .captures
                .iter()
                .zip(&wire.captures)
                .all(|((name, value), cap)| {
                    *name == cap.name.as_str() && value.as_str() == cap.value.as_str()
                })
    }
}

impl From<&LogicalId> for wit::LogicalId {
    fn from(id: &LogicalId) -> Self {
        id.clone().into()
    }
}

/// A route-context capture excluded from identity. `Deref` so handlers
/// read `*facet`.
///
/// Wrap a key field in `Facet` when the segment selects a view of the
/// object rather than a different object: every value of the facet then
/// resolves to the same [`LogicalId`] and shares one cached canonical
/// (e.g. `/{paper}/@latest` and `/{paper}/v3` both load the paper once).
/// When the facet's type exposes a finite [`crate::captures::PathSegment::choices`]
/// set, the SDK also expands view leaves across every choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Facet<T>(pub T);

impl<T> Deref for Facet<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_wire_self_check() {
        let id = LogicalId::new(
            ObjectKind("github.issue"),
            vec![("owner", "o".into()), ("number", "42".into())],
        );
        let wire: wit::LogicalId = (&id).into();
        assert!(id.matches_wire(&wire));
        let other = LogicalId::new(
            ObjectKind("github.issue"),
            vec![("owner", "o".into()), ("number", "99".into())],
        );
        assert!(!other.matches_wire(&wire));
    }
}
