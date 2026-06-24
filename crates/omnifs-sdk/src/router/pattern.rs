//! Route template grammar, matching, precedence, and ambiguity.
//!
//! This module owns the route-pattern types ([`Pattern`], [`CaptureLocation`],
//! [`Match`], [`Error`]). Route matching is SDK-side knowledge; no host, fuse,
//! or CLI code consumes these types.
//!
//! # Template grammar
//!
//! A template is an absolute path (`/` alone is the empty pattern). No
//! trailing slash, no empty segments, no `.` or `..`. Each segment is one of:
//!
//! - a literal: `issues`, `repo.json`
//! - a bare capture: `{owner}` (matches any non-empty segment)
//! - a prefix capture: `@{resolver}`, `v{version}` (a non-empty literal
//!   prefix glued to a capture in one segment; matches segments that start
//!   with the prefix and have a non-empty remainder, and the capture value
//!   is the remainder with the prefix stripped)
//! - a rest capture: `{*rest}` (multi-segment; only legal as the final
//!   segment, and matches zero or more trailing segments joined by `/`)
//!
//! Capture names must be non-empty identifiers: `[A-Za-z_][A-Za-z0-9_]*`.
//!
//! # Precedence
//!
//! [`Pattern::precedence_key`] orders candidate routes when several match the
//! same concrete path: any non-rest pattern beats any rest pattern, then more
//! literal segments win, then more prefix captures, then more segments.
//! Dispatch sorts candidates by this key and takes the head (see
//! [`best_match`]).
//!
//! # Ambiguity
//!
//! [`Pattern::is_ambiguous_with`] detects two leaf claims that can bind the
//! same concrete path with equal precedence, where neither could reliably win.
//! [`Router::seal`](super::Router::seal) runs this pairwise over all leaf
//! claims and fails initialization on the first overlap, so shadowed routes
//! are a startup error rather than silent runtime behavior.

use super::handlers::{DirEntry, FileEntry, RouteValidator, TreeRefEntry};
use crate::captures::Captures;
use crate::error::{ProviderError, Result};
use omnifs_core::path::{Path, Segment};

// Within this module, `Result` from crate::error is `Result<T, ProviderError>`.
// Pattern parsing returns `Result<T, Error>` using the stdlib alias directly.

// ===========================================================================
// Pattern types
// ===========================================================================

/// A compiled route template, e.g. `/{owner}/{repo}/issues/{number}`.
///
/// Compilation precomputes the counts that feed [`Self::precedence_key`], so
/// matching and ordering are comparisons over prebuilt data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pattern {
    segments: Vec<PatternSegment>,
    literal_count: usize,
    prefix_capture_count: usize,
    has_rest: bool,
}

/// Where a named capture lives within a [`Pattern`] and what prefix (if any)
/// it strips from the raw segment before yielding the capture value.
///
/// The inverse direction matters too: [`Self::render_segment`] re-applies the
/// prefix when a capture value is substituted back into a concrete path
/// (facet view-leaf expansion relies on this round trip).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureLocation {
    segment_index: usize,
    prefix: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PatternSegment {
    Literal(String),
    Capture {
        name: String,
        prefix: Option<String>,
    },
    Rest {
        name: String,
    },
}

/// A pattern parse or match error.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct Error {
    message: String,
}

/// The result of a successful pattern match: the matched path plus its
/// decoded captures in template order. Prefix-capture values arrive with the
/// prefix already stripped (`@google` yields `google`); a rest capture yields
/// the trailing segments joined by `/` (empty string for zero segments).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    path: Path,
    captures: Vec<Capture>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Capture {
    name: String,
    value: String,
}

impl Error {
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl From<String> for Error {
    fn from(message: String) -> Self {
        Self { message }
    }
}

impl CaptureLocation {
    #[must_use]
    pub fn segment_index(&self) -> usize {
        self.segment_index
    }

    #[must_use]
    pub fn render_segment(&self, value: &str) -> String {
        match &self.prefix {
            Some(prefix) => format!("{prefix}{value}"),
            None => value.to_string(),
        }
    }
}

impl Match {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.captures
            .iter()
            .find(|capture| capture.name == name)
            .map(|capture| capture.value.as_str())
    }

    pub fn captures(&self) -> impl Iterator<Item = (&str, &str)> {
        self.captures
            .iter()
            .map(|capture| (capture.name.as_str(), capture.value.as_str()))
    }
}

impl Pattern {
    /// Compile a template (see the module docs for the grammar).
    ///
    /// Rejects: relative templates, trailing `/`, empty segments (`//`),
    /// `.`/`..` segments, non-identifier capture names, a `{*rest}` capture
    /// anywhere but last, and malformed capture syntax (nested or unclosed
    /// braces, empty prefix). `"/"` compiles to the empty pattern that matches
    /// only the root path.
    pub fn parse(template: &str) -> core::result::Result<Self, Error> {
        if template == "/" {
            return Ok(Self {
                segments: Vec::new(),
                literal_count: 0,
                prefix_capture_count: 0,
                has_rest: false,
            });
        }
        if !template.starts_with('/') || template.ends_with('/') || template.contains("//") {
            return Err(format!("invalid path template {template:?}").into());
        }

        let raw_segments: Vec<&str> = template.split('/').skip(1).collect();
        let mut segments = Vec::with_capacity(raw_segments.len());
        let mut literal_count = 0usize;
        let mut prefix_capture_count = 0usize;
        let mut has_rest = false;
        let total = raw_segments.len();

        for (index, raw) in raw_segments.into_iter().enumerate() {
            if raw.is_empty() || matches!(raw, "." | "..") {
                return Err(format!("invalid path template segment {raw:?}").into());
            }
            if raw.starts_with("{*") {
                if !raw.ends_with('}') || raw.len() < 4 {
                    return Err(format!("invalid rest-capture segment {raw:?}").into());
                }
                if index != total - 1 {
                    return Err(format!(
                        "rest-capture segment {raw:?} must be the last segment of the pattern"
                    )
                    .into());
                }
                let name = &raw[2..raw.len() - 1];
                validate_capture_name(name)?;
                segments.push(PatternSegment::Rest {
                    name: name.to_string(),
                });
                has_rest = true;
                continue;
            }
            if raw.starts_with('{') && raw.ends_with('}') {
                let name = &raw[1..raw.len() - 1];
                validate_capture_name(name)?;
                segments.push(PatternSegment::Capture {
                    name: name.to_string(),
                    prefix: None,
                });
                continue;
            }
            if let Some(start) = raw.find('{') {
                if !raw.ends_with('}') || raw[start + 1..raw.len() - 1].contains('{') {
                    return Err(format!("invalid capture segment {raw:?}").into());
                }
                let prefix = &raw[..start];
                if prefix.is_empty() || prefix.contains('/') {
                    return Err(format!("invalid capture prefix in segment {raw:?}").into());
                }
                let name = &raw[start + 1..raw.len() - 1];
                validate_capture_name(name)?;
                prefix_capture_count += 1;
                segments.push(PatternSegment::Capture {
                    name: name.to_string(),
                    prefix: Some(prefix.to_string()),
                });
                continue;
            }
            literal_count += 1;
            segments.push(PatternSegment::Literal(raw.to_string()));
        }

        Ok(Self {
            segments,
            literal_count,
            prefix_capture_count,
            has_rest,
        })
    }

    /// The route-ordering key, compared descending by dispatch: non-rest
    /// beats rest, then literal-segment count, then prefix-capture count,
    /// then segment count. Patterns with equal keys cannot be ordered, which
    /// is exactly the case [`Self::is_ambiguous_with`] exists to reject.
    #[must_use]
    pub fn precedence_key(&self) -> (u8, usize, usize, usize) {
        let is_not_rest = u8::from(!self.has_rest);
        (
            is_not_rest,
            self.literal_count,
            self.prefix_capture_count,
            self.segments.len(),
        )
    }

    /// Match a whole concrete path. Non-rest patterns require an exact
    /// segment-count match; a rest pattern matches its fixed prefix plus zero
    /// or more trailing segments (so `/ipfs/{cid}/{*path}` matches
    /// `/ipfs/Qm123` itself).
    pub fn match_path(&self, path: &Path) -> core::result::Result<Match, Error> {
        let segments: Vec<&str> = path.segments().collect();
        if self.has_rest {
            let fixed = self.fixed_prefix_len();
            if segments.len() < fixed || !self.matches_prefix_segments(&segments[..fixed]) {
                return Err(format!("path {path:?} does not match pattern").into());
            }
        } else if segments.len() != self.segments.len() || !self.matches_prefix_segments(&segments)
        {
            return Err(format!("path {path:?} does not match pattern").into());
        }

        Ok(Match {
            path: path.clone(),
            captures: self.captures_from_segments(&segments),
        })
    }

    /// Match a path that may stop partway through the pattern (an ancestor
    /// probe). Captures are decoded only for the segments actually present;
    /// callers must tolerate missing captures (see
    /// [`crate::captures::FromCaptures::validate_present_captures`]).
    pub fn match_prefix(&self, path: &Path) -> core::result::Result<Match, Error> {
        let segments: Vec<&str> = path.segments().collect();
        if !self.has_rest && segments.len() > self.segments.len() {
            return Err(format!("path {path:?} is longer than pattern prefix").into());
        }
        let comparable = if self.has_rest && segments.len() > self.fixed_prefix_len() {
            &segments[..self.fixed_prefix_len()]
        } else {
            segments.as_slice()
        };
        if !self.matches_prefix_segments(comparable) {
            return Err(format!("path {path:?} does not match pattern prefix").into());
        }
        Ok(Match {
            path: path.clone(),
            captures: self.captures_from_segments(&segments),
        })
    }

    /// Whether `parent_segments` is a proper ancestor of paths this pattern
    /// can match, i.e. the pattern extends at least one segment below it.
    /// This is the candidacy test for auto-navigable intermediate
    /// directories.
    #[must_use]
    pub fn accepts_as_strict_ancestor(&self, parent_segments: &[&str]) -> bool {
        parent_segments.len() < self.segments.len() && self.matches_prefix_segments(parent_segments)
    }

    /// The literal child name this pattern contributes directly under
    /// `parent_segments`, plus whether the pattern continues below that child
    /// (`true` means the child is an intermediate directory, not this
    /// pattern's leaf). `None` when the next segment is dynamic or the parent
    /// is not a proper ancestor. This is how static directory discovery
    /// synthesizes literal entries without running any handler.
    #[must_use]
    pub fn literal_child_after<'a>(&'a self, parent_segments: &[&str]) -> Option<(&'a str, bool)> {
        if !self.accepts_as_strict_ancestor(parent_segments) {
            return None;
        }
        let parent_depth = parent_segments.len();
        let PatternSegment::Literal(name) = &self.segments[parent_depth] else {
            return None;
        };
        Some((name.as_str(), self.segments.len() > parent_depth + 1))
    }

    /// Whether the segment directly under `parent_segments` is a capture or
    /// rest segment. A `true` from any route at a given depth makes that
    /// directory's static listing non-exhaustive: names this pattern would
    /// accept cannot be enumerated.
    #[must_use]
    pub fn has_dynamic_child_after(&self, parent_segments: &[&str]) -> bool {
        if !self.accepts_as_strict_ancestor(parent_segments) {
            return false;
        }
        matches!(
            self.segments[parent_segments.len()],
            PatternSegment::Capture { .. } | PatternSegment::Rest { .. }
        )
    }

    /// A shape signature of all segments but the last (`l:<lit>`, `p:<prefix>`,
    /// `c`, `r` joined by `/`). Used only for human-readable overlap
    /// diagnostics in [`Router::seal`](super::Router::seal) errors; capture
    /// names are deliberately erased because they do not affect matching.
    #[must_use]
    pub fn parent_signature(&self) -> String {
        self.segments
            .iter()
            .take(self.segments.len().saturating_sub(1))
            .map(segment_signature)
            .collect::<Vec<_>>()
            .join("/")
    }

    #[must_use]
    pub fn pattern_len(&self) -> usize {
        self.segments.len()
    }

    /// Locate a named single-segment capture. Rest captures have no location:
    /// they span a variable number of segments and cannot be substituted.
    #[must_use]
    pub fn capture_location(&self, name: &str) -> Option<CaptureLocation> {
        self.segments
            .iter()
            .enumerate()
            .find_map(|(segment_index, segment)| match segment {
                PatternSegment::Capture {
                    name: capture_name,
                    prefix,
                } if capture_name == name => Some(CaptureLocation {
                    segment_index,
                    prefix: prefix.clone(),
                }),
                PatternSegment::Literal(_)
                | PatternSegment::Capture { .. }
                | PatternSegment::Rest { .. } => None,
            })
    }

    /// Segment count excluding a trailing rest capture: the part of the
    /// pattern that must be present for any match.
    #[must_use]
    pub fn fixed_prefix_len(&self) -> usize {
        if self.has_rest {
            self.segments.len() - 1
        } else {
            self.segments.len()
        }
    }

    /// Whether two leaf claims can bind the same concrete path with equal
    /// precedence, so neither would reliably win dispatch.
    ///
    /// Two non-rest patterns are ambiguous when they have equal
    /// [`Self::precedence_key`]s and every segment pair overlaps (literal vs
    /// equal literal, anything vs bare capture, literal vs prefix capture
    /// whose prefix it extends, prefix captures where one prefix extends the
    /// other). Two rest patterns are ambiguous when their fixed prefixes have
    /// equal length and overlap pairwise. A rest pattern is never ambiguous
    /// with a non-rest pattern: precedence always separates them.
    ///
    /// Note the asymmetry with dispatch: routes that merely overlap but have
    /// different precedence keys (`/{owner}` vs `/resolvers`) are legal and
    /// resolve by specificity; only equal-precedence overlap is rejected.
    #[must_use]
    pub fn is_ambiguous_with(&self, other: &Self) -> bool {
        match (self.has_rest, other.has_rest) {
            (true, true) => {
                self.fixed_prefix_len() == other.fixed_prefix_len()
                    && self
                        .segments
                        .iter()
                        .take(self.fixed_prefix_len())
                        .zip(other.segments.iter().take(other.fixed_prefix_len()))
                        .all(|(left, right)| segments_overlap(left, right))
            },
            (true, false) | (false, true) => false,
            (false, false) => {
                self.precedence_key() == other.precedence_key()
                    && self.segments.len() == other.segments.len()
                    && self
                        .segments
                        .iter()
                        .zip(other.segments.iter())
                        .all(|(left, right)| segments_overlap(left, right))
            },
        }
    }

    fn captures_from_segments(&self, concrete: &[&str]) -> Vec<Capture> {
        let mut captures = Vec::new();
        for (index, segment) in self.segments.iter().enumerate() {
            match segment {
                PatternSegment::Literal(_) => {},
                PatternSegment::Capture { name, prefix } => {
                    let Some(raw) = concrete.get(index) else {
                        continue;
                    };
                    let value = match prefix {
                        Some(prefix) => raw.strip_prefix(prefix.as_str()).unwrap_or(raw),
                        None => raw,
                    };
                    captures.push(Capture {
                        name: name.clone(),
                        value: value.to_string(),
                    });
                },
                PatternSegment::Rest { name } => {
                    if index > concrete.len() {
                        continue;
                    }
                    captures.push(Capture {
                        name: name.clone(),
                        value: concrete[index..].join("/"),
                    });
                },
            }
        }
        captures
    }

    fn matches_prefix_segments(&self, concrete: &[&str]) -> bool {
        self.segments
            .iter()
            .take(concrete.len())
            .zip(concrete.iter().copied())
            .all(|(pattern, actual)| match pattern {
                PatternSegment::Literal(expected) => actual == expected,
                PatternSegment::Capture { prefix: None, .. } => !actual.is_empty(),
                PatternSegment::Capture {
                    prefix: Some(prefix),
                    ..
                } => actual
                    .strip_prefix(prefix)
                    .is_some_and(|rest| !rest.is_empty()),
                PatternSegment::Rest { .. } => true,
            })
    }
}

fn validate_capture_name(name: &str) -> core::result::Result<(), Error> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err("capture names cannot be empty".to_string().into());
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return Err(format!("invalid capture name {name:?}").into());
    }
    if chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric()) {
        Ok(())
    } else {
        Err(format!("invalid capture name {name:?}").into())
    }
}

fn segment_signature(segment: &PatternSegment) -> String {
    match segment {
        PatternSegment::Literal(value) => format!("l:{value}"),
        PatternSegment::Capture {
            prefix: Some(prefix),
            ..
        } => format!("p:{prefix}"),
        PatternSegment::Capture { prefix: None, .. } => "c".to_string(),
        PatternSegment::Rest { .. } => "r".to_string(),
    }
}

fn segments_overlap(left: &PatternSegment, right: &PatternSegment) -> bool {
    if matches!(left, PatternSegment::Rest { .. }) || matches!(right, PatternSegment::Rest { .. }) {
        return true;
    }
    match (left, right) {
        (PatternSegment::Literal(left), PatternSegment::Literal(right)) => left == right,
        (
            PatternSegment::Literal(_) | PatternSegment::Capture { .. },
            PatternSegment::Capture { prefix: None, .. },
        )
        | (
            PatternSegment::Capture { prefix: None, .. },
            PatternSegment::Literal(_) | PatternSegment::Capture { .. },
        ) => true,
        (
            PatternSegment::Literal(literal),
            PatternSegment::Capture {
                prefix: Some(prefix),
                ..
            },
        )
        | (
            PatternSegment::Capture {
                prefix: Some(prefix),
                ..
            },
            PatternSegment::Literal(literal),
        ) => literal
            .strip_prefix(prefix)
            .is_some_and(|rest| !rest.is_empty()),
        (
            PatternSegment::Capture {
                prefix: Some(left), ..
            },
            PatternSegment::Capture {
                prefix: Some(right),
                ..
            },
        ) => left.starts_with(right) || right.starts_with(left),
        (PatternSegment::Rest { .. }, _) | (_, PatternSegment::Rest { .. }) => unreachable!(),
    }
}

// ===========================================================================
// Route table helpers
// ===========================================================================

/// Route table row: pattern plus per-route capture validator.
pub(super) trait RoutedEntry {
    fn route_pattern(&self) -> &Pattern;
    fn route_validator(&self) -> &RouteValidator;
}

impl<S> RoutedEntry for DirEntry<S> {
    fn route_pattern(&self) -> &Pattern {
        &self.pattern
    }
    fn route_validator(&self) -> &RouteValidator {
        &self.validator
    }
}

impl<S> RoutedEntry for FileEntry<S> {
    fn route_pattern(&self) -> &Pattern {
        &self.pattern
    }
    fn route_validator(&self) -> &RouteValidator {
        &self.validator
    }
}

impl<S> RoutedEntry for TreeRefEntry<S> {
    fn route_pattern(&self) -> &Pattern {
        &self.pattern
    }
    fn route_validator(&self) -> &RouteValidator {
        &self.validator
    }
}

impl<S> RoutedEntry for super::object::ObjectRouteEntry<S> {
    fn route_pattern(&self) -> &Pattern {
        &self.pattern
    }
    fn route_validator(&self) -> &RouteValidator {
        &self.validator
    }
}

impl<S> RoutedEntry for &DirEntry<S> {
    fn route_pattern(&self) -> &Pattern {
        &self.pattern
    }
    fn route_validator(&self) -> &RouteValidator {
        &self.validator
    }
}

impl<S> RoutedEntry for &FileEntry<S> {
    fn route_pattern(&self) -> &Pattern {
        &self.pattern
    }
    fn route_validator(&self) -> &RouteValidator {
        &self.validator
    }
}

impl<S> RoutedEntry for &super::object::ObjectRouteEntry<S> {
    fn route_pattern(&self) -> &Pattern {
        &self.pattern
    }
    fn route_validator(&self) -> &RouteValidator {
        &self.validator
    }
}

/// Highest-precedence route whose pattern matches `abs` and whose validator
/// accepts the decoded captures.
///
/// The validator runs during candidacy, before precedence sorting. This is
/// where capture-parse fallthrough happens: a route whose typed key rejects
/// the segment (e.g. `{number}` fed a non-numeric value) is filtered out
/// here, so a less specific sibling route can still win instead of the path
/// resolving to not-found.
pub(super) fn best_match<'a, E, I>(routes: I, abs: &Path) -> Option<(&'a E, Captures)>
where
    E: RoutedEntry + 'a,
    I: IntoIterator<Item = &'a E>,
{
    let mut candidates: Vec<(&E, Captures)> = routes
        .into_iter()
        .filter_map(|route| {
            let matched = route.route_pattern().match_path(abs).ok()?;
            let caps = Captures::from_match(&matched);
            route
                .route_validator()
                .accepts(&caps)
                .then_some((route, caps))
        })
        .collect();
    candidates.sort_by(|a, b| {
        b.0.route_pattern()
            .precedence_key()
            .cmp(&a.0.route_pattern().precedence_key())
    });
    candidates.into_iter().next()
}

pub(crate) fn parse_pattern(template: &str) -> Result<Pattern> {
    Pattern::parse(template)
        .map_err(|error| ProviderError::invalid_input(error.message().to_string()))
}

pub(super) fn parse_provider_path(path: &str) -> Result<Path> {
    Path::parse(path).map_err(|error| ProviderError::invalid_input(error.to_string()))
}

pub(super) fn parse_child_segment(name: &str) -> Result<Segment> {
    Segment::try_from(name).map_err(|error| ProviderError::invalid_input(error.to_string()))
}

#[cfg(test)]
mod pattern_tests {
    use super::{Path, Pattern};

    #[test]
    fn match_path_decodes_captures() {
        let pattern = Pattern::parse("/@{resolver}/{segment}/{*tail}").unwrap();
        let matched = pattern
            .match_path(&Path::parse("/@google/example.com/a/b").unwrap())
            .unwrap();

        assert_eq!(matched.path().as_str(), "/@google/example.com/a/b");
        assert_eq!(matched.get("resolver"), Some("google"));
        assert_eq!(matched.get("segment"), Some("example.com"));
        assert_eq!(matched.get("tail"), Some("a/b"));
        assert_eq!(
            matched.captures().collect::<Vec<_>>(),
            vec![
                ("resolver", "google"),
                ("segment", "example.com"),
                ("tail", "a/b")
            ]
        );
    }

    #[test]
    fn capture_location_preserves_segment_prefix() {
        let pattern = Pattern::parse("/@{resolver}/{domain}/{record}").unwrap();
        let resolver = pattern.capture_location("resolver").unwrap();
        let domain = pattern.capture_location("domain").unwrap();

        assert_eq!(resolver.segment_index(), 0);
        assert_eq!(resolver.render_segment("google"), "@google");
        assert_eq!(domain.segment_index(), 1);
        assert_eq!(domain.render_segment("example.com"), "example.com");
        assert!(pattern.capture_location("missing").is_none());
    }

    #[test]
    fn rest_capture_matches_zero_or_more_trailing_segments() {
        let pat = Pattern::parse("/ipfs/{cid}/{*path}").unwrap();
        assert!(pat.match_path(&Path::parse("/ipfs/Qm123").unwrap()).is_ok());
        assert!(
            pat.match_path(&Path::parse("/ipfs/Qm123/a").unwrap())
                .is_ok()
        );
        assert!(
            pat.match_path(&Path::parse("/ipfs/Qm123/a/b/c").unwrap())
                .is_ok()
        );
        assert!(pat.match_path(&Path::parse("/ipfs").unwrap()).is_err());
        assert!(
            pat.match_path(&Path::parse("/other/Qm123").unwrap())
                .is_err()
        );
    }

    #[test]
    fn rest_capture_has_lowest_precedence() {
        let rest = Pattern::parse("/ipfs/{cid}/{*path}").unwrap();
        let bare = Pattern::parse("/ipfs/{cid}/{leaf}").unwrap();
        let prefix = Pattern::parse("/ipfs/{cid}/v{version}").unwrap();
        let exact = Pattern::parse("/ipfs/{cid}/versions").unwrap();

        assert!(exact.precedence_key() > prefix.precedence_key());
        assert!(prefix.precedence_key() > bare.precedence_key());
        assert!(bare.precedence_key() > rest.precedence_key());
    }

    #[test]
    fn rest_capture_ambiguity_rules() {
        let rest_a = Pattern::parse("/ipfs/{cid}/{*path}").unwrap();
        let rest_b = Pattern::parse("/ipfs/{cid}/{*tail}").unwrap();
        let bare = Pattern::parse("/ipfs/{cid}/{leaf}").unwrap();
        let exact = Pattern::parse("/ipfs/{cid}/versions").unwrap();
        let other_rest = Pattern::parse("/other/{id}/{*rest}").unwrap();

        assert!(rest_a.is_ambiguous_with(&rest_b));
        assert!(rest_b.is_ambiguous_with(&rest_a));
        assert!(!rest_a.is_ambiguous_with(&bare));
        assert!(!bare.is_ambiguous_with(&rest_a));
        assert!(!rest_a.is_ambiguous_with(&exact));
        assert!(!rest_a.is_ambiguous_with(&other_rest));
    }

    #[test]
    fn parent_child_helpers_hide_segment_representation() {
        let pattern = Pattern::parse("/owners/{owner}/repos/settings").unwrap();
        let parent = ["owners", "rust-lang", "repos"];
        assert_eq!(
            pattern.literal_child_after(&parent),
            Some(("settings", false))
        );
        assert!(!pattern.has_dynamic_child_after(&parent));

        let dynamic = Pattern::parse("/owners/{owner}/repos/{repo}").unwrap();
        assert!(dynamic.has_dynamic_child_after(&parent));
        assert_eq!(dynamic.literal_child_after(&parent), None);
    }
}
