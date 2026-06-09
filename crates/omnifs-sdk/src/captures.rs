//! Path captures (ADR-0001 §8, §10).
//!
//! A [`PathSegment`] validates one path segment and may enumerate a finite
//! choice set. A multi-segment key is a `#[path_captures]` struct whose
//! generated [`FromCaptures`] impl parses each field from the matched
//! segments. The route engine binds segment values into [`Captures`] before
//! a handler or `Object::load` runs.

use crate::error::{ProviderError, Result};
use crate::router::pattern::{Match as PatternMatch, Pattern};
use core::fmt::Display;
use core::str::FromStr;
use omnifs_core::path::Path;

/// A capture type: validates one path segment (`FromStr`), renders back
/// (`Display`), and optionally enumerates a finite child set.
///
/// A `Some(..)` choice set enumerates a directory's children without a
/// directory projection; `None` is an unbounded capture.
pub trait PathSegment: FromStr + Display + 'static {
    /// The finite set of allowed values, or `None` for an unbounded capture.
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

    /// Raw value for the capture at position `index` (template order).
    pub fn nth(&self, index: usize) -> Option<&str> {
        self.items.get(index).map(|c| c.value.as_str())
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Build captures from a successful route pattern match.
    pub fn from_match(matched: &PatternMatch) -> Self {
        Self::new(
            matched
                .captures()
                .map(|(name, value)| Capture {
                    name: name.to_string(),
                    value: value.to_string(),
                })
                .collect(),
        )
    }

    /// Build captures for a concrete path against a route pattern.
    pub fn from_pattern_match(pattern: &Pattern, concrete_path: &str) -> Self {
        let Ok(path) = Path::parse(concrete_path) else {
            return Self::new(Vec::new());
        };
        pattern.match_path(&path).map_or_else(
            |_| Self::new(Vec::new()),
            |matched| Self::from_match(&matched),
        )
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
