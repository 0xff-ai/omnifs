//! Path captures (ADR-0001 §8, §10).
//!
//! A [`PathSegment`] validates one path segment and may enumerate a finite
//! choice set. A multi-segment key is a `#[path_captures]` struct whose
//! generated [`FromCaptures`] impl parses each field from the matched
//! segments. The route engine binds segment values into [`Captures`] before
//! a handler or `Object::load` runs.

use crate::error::{ProviderError, Result};
use core::fmt::Display;
use core::str::FromStr;

/// A capture type: validates one path segment (`FromStr`), renders back
/// (`Display`), and optionally declares a finite value set.
///
/// The `FromStr` impl is the segment validator: a parse rejection makes
/// the route a non-candidate, falling through to the next-most-specific
/// route rather than to "not found". `Display` must round-trip what
/// `FromStr` accepted, because rendered values become path segments again
/// (see how arxiv percent-encodes paper ids in `Display` and decodes in
/// `FromStr`).
pub trait PathSegment: FromStr + Display + 'static {
    /// The finite set of allowed values, or `None` for an unbounded
    /// capture.
    ///
    /// For a `Some(..)` set, keep `FromStr` consistent: accept exactly the
    /// listed values. When such a type sits in a [`crate::identity::Facet`]
    /// key field, the `#[path_captures]` macro turns the set into a
    /// [`crate::object::FacetAxis`] and the SDK expands canonical-store
    /// view leaves across every choice, so all facet values are served
    /// from one cached object.
    fn choices() -> Option<&'static [&'static str]> {
        None
    }
}

/// Implemented by `#[path_captures]` structs. The macro checks the struct's
/// field names against the route template and generates this impl.
pub trait FromCaptures: Sized {
    fn from_captures(caps: &Captures) -> Result<Self>;

    /// Validate the captures already present in a path prefix.
    ///
    /// Static directory discovery walks route prefixes before every capture in
    /// the full route is available. Full dispatch still calls
    /// [`Self::from_captures`]; this hook only prevents a future missing
    /// capture from hiding a literal ancestor such as `/categories`.
    fn validate_present_captures(caps: &Captures) -> bool {
        Self::from_captures(caps).is_ok()
    }
}

/// A single capture matched from a path: its template name and the raw
/// segment value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capture {
    pub name: String,
    pub value: String,
}

/// The ordered set of captures the route engine matched for a path, keyed by
/// the template field name (`owner`, `repo`, `number`). Values are raw
/// segment strings; [`Captures::parse`] applies the field's `FromStr`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Captures {
    items: Vec<Capture>,
}

impl Captures {
    pub fn new(items: Vec<Capture>) -> Self {
        Self { items }
    }

    /// Raw value for a named capture.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.items
            .iter()
            .find(|c| c.name == name)
            .map(|c| c.value.as_str())
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Parse a named capture through its `FromStr`, mapping a parse failure
    /// to an invalid-input error naming the capture.
    pub fn parse<T>(&self, name: &str) -> Result<T>
    where
        T: FromStr,
    {
        let raw = self
            .get(name)
            .ok_or_else(|| ProviderError::invalid_input(format!("missing capture {name:?}")))?;
        raw.parse::<T>()
            .map_err(|_| ProviderError::invalid_input(format!("invalid capture {name:?}: {raw:?}")))
    }

    /// Parse a named capture that may be absent, for an `Option<T>` key field
    /// shared across routes with and without the segment (e.g. a paper key used
    /// at both `/{paper}/paper.pdf` and `/{paper}/versions/{version}/paper.pdf`).
    /// `Ok(None)` when the capture is absent; a present-but-invalid value is
    /// still an error, so the route validator rejects it.
    pub fn parse_optional<T>(&self, name: &str) -> Result<Option<T>>
    where
        T: FromStr,
    {
        match self.get(name) {
            None => Ok(None),
            Some(raw) => raw.parse::<T>().map(Some).map_err(|_| {
                ProviderError::invalid_input(format!("invalid capture {name:?}: {raw:?}"))
            }),
        }
    }
}
