//! Path pattern parsing and match helpers.
//!
//! This module owns the route-pattern types (`Pattern`, `CaptureLocation`,
//! `Match`, `Error`) that were originally in `omnifs-core`. They moved here
//! because route matching is SDK-side knowledge; no host, fuse, or CLI code
//! consumes these types.

use super::handlers::{DirEntry, FileEntry, RouteValidator, TreeRefEntry};
use crate::captures::Captures;
use crate::error::{ProviderError, Result};
use omnifs_core::path::{Path, Segment};

// Within this module, `Result` from crate::error is `Result<T, ProviderError>`.
// Pattern parsing returns `Result<T, Error>` using the stdlib alias directly.

// ===========================================================================
// Pattern types
// ===========================================================================

/// A compiled route pattern, e.g. `/{owner}/{repo}/issues/{number}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pattern {
    segments: Vec<PatternSegment>,
    literal_count: usize,
    prefix_capture_count: usize,
    has_rest: bool,
    specificity: Vec<(u8, usize)>,
}

/// Where a named capture lives within a `Pattern` and what prefix (if any)
/// it strips from the raw segment before yielding the capture value.
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
/// decoded captures.
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

    #[must_use]
    pub fn into_path(self) -> Path {
        self.path
    }
}

impl Pattern {
    pub fn parse(template: &str) -> core::result::Result<Self, Error> {
        if template == "/" {
            return Ok(Self {
                segments: Vec::new(),
                literal_count: 0,
                prefix_capture_count: 0,
                has_rest: false,
                specificity: Vec::new(),
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

        let specificity = segments.iter().map(segment_specificity).collect();
        Ok(Self {
            segments,
            literal_count,
            prefix_capture_count,
            has_rest,
            specificity,
        })
    }

    #[must_use]
    pub fn has_rest(&self) -> bool {
        self.has_rest
    }

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

    #[must_use]
    pub fn matches_path(&self, path: &Path) -> bool {
        self.match_path(path).is_ok()
    }

    #[must_use]
    pub fn accepts_as_strict_ancestor(&self, parent_segments: &[&str]) -> bool {
        parent_segments.len() < self.segments.len() && self.matches_prefix_segments(parent_segments)
    }

    #[must_use]
    pub fn matches_parent_path(&self, path: &Path) -> bool {
        let segments: Vec<&str> = path.segments().collect();
        if self.has_rest {
            let fixed = self.fixed_prefix_len();
            segments.len() >= fixed && self.matches_prefix_segments(&segments[..fixed])
        } else {
            segments.len() + 1 == self.segments.len() && self.matches_prefix_segments(&segments)
        }
    }

    #[must_use]
    pub fn static_child(&self) -> Option<&str> {
        match self.segments.last()? {
            PatternSegment::Literal(name) => Some(name),
            PatternSegment::Capture { .. } | PatternSegment::Rest { .. } => None,
        }
    }

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
    pub fn concrete_path_for(&self, concrete_path: &Path) -> Option<Path> {
        let segments: Vec<&str> = concrete_path.segments().collect();
        if self.has_rest {
            let fixed = self.fixed_prefix_len();
            if segments.len() < fixed || !self.matches_prefix_segments(&segments[..fixed]) {
                return None;
            }
            Some(join_absolute_path(&segments))
        } else {
            if self.segments.len() > segments.len() || !self.matches_prefix_segments(&segments) {
                return None;
            }
            Some(join_absolute_path(&segments[..self.segments.len()]))
        }
    }

    #[must_use]
    pub fn matches_exact_path(&self, concrete_path: &Path) -> bool {
        self.concrete_path_for(concrete_path)
            .is_some_and(|matched| matched == *concrete_path)
    }

    #[must_use]
    pub fn pattern_len(&self) -> usize {
        self.segments.len()
    }

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

    #[must_use]
    pub fn fixed_prefix_len(&self) -> usize {
        if self.has_rest {
            self.segments.len() - 1
        } else {
            self.segments.len()
        }
    }

    #[must_use]
    pub fn specificity(&self) -> &[(u8, usize)] {
        &self.specificity
    }

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

    #[must_use]
    pub fn rest_of(&self, path: &Path) -> Option<String> {
        if !self.has_rest {
            return None;
        }
        let segments: Vec<&str> = path.segments().collect();
        let fixed = self.fixed_prefix_len();
        if segments.len() < fixed || !self.matches_prefix_segments(&segments[..fixed]) {
            return None;
        }
        Some(segments[fixed..].join("/"))
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

fn join_absolute_path(segments: &[&str]) -> Path {
    if segments.is_empty() {
        Path::root()
    } else {
        Path::from_validated(format!("/{}", segments.join("/")))
    }
}

fn segment_specificity(segment: &PatternSegment) -> (u8, usize) {
    match segment {
        PatternSegment::Literal(value) => (2, value.len()),
        PatternSegment::Capture {
            prefix: Some(prefix),
            ..
        } => (1, prefix.len()),
        PatternSegment::Capture { prefix: None, .. } | PatternSegment::Rest { .. } => (0, 0),
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

impl<S> RoutedEntry for super::object::ObjectEntry<S> {
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

impl<S> RoutedEntry for &super::object::ObjectEntry<S> {
    fn route_pattern(&self) -> &Pattern {
        &self.pattern
    }
    fn route_validator(&self) -> &RouteValidator {
        &self.validator
    }
}

/// Longest-precedence route from an iterator whose pattern matches `abs` and
/// whose validator accepts captures.
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
    fn pattern_matches_and_prefers_literals() {
        let repo = Pattern::parse("/{owner}/{repo}").unwrap();
        let issue = Pattern::parse("/{owner}/{repo}/issues/open/{number}").unwrap();
        let resolver = Pattern::parse("/@{resolver}/{segment}").unwrap();
        let literal = Pattern::parse("/resolvers").unwrap();
        let capture = Pattern::parse("/{segment}").unwrap();
        let concrete = Path::parse("/openai/gvfs/issues/open/7").unwrap();

        assert_eq!(
            repo.concrete_path_for(&concrete),
            Some(Path::parse("/openai/gvfs").unwrap())
        );
        assert_eq!(
            issue.concrete_path_for(&Path::parse("/openai/gvfs/issues/open/7/comments/1").unwrap()),
            Some(Path::parse("/openai/gvfs/issues/open/7").unwrap())
        );
        assert_eq!(
            resolver.concrete_path_for(&Path::parse("/@google/example.com").unwrap()),
            Some(Path::parse("/@google/example.com").unwrap())
        );
        assert_eq!(
            resolver.concrete_path_for(&Path::parse("/@google").unwrap()),
            None
        );
        assert!(literal.specificity() > capture.specificity());
    }

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
        assert!(pat.matches_path(&Path::parse("/ipfs/Qm123").unwrap()));
        assert!(pat.matches_path(&Path::parse("/ipfs/Qm123/a").unwrap()));
        assert!(pat.matches_path(&Path::parse("/ipfs/Qm123/a/b/c").unwrap()));
        assert!(!pat.matches_path(&Path::parse("/ipfs").unwrap()));
        assert!(!pat.matches_path(&Path::parse("/other/Qm123").unwrap()));

        assert_eq!(
            pat.rest_of(&Path::parse("/ipfs/Qm123").unwrap()),
            Some(String::new())
        );
        assert_eq!(
            pat.rest_of(&Path::parse("/ipfs/Qm123/a").unwrap()),
            Some("a".to_string())
        );
        assert_eq!(
            pat.rest_of(&Path::parse("/ipfs/Qm123/a/b/c").unwrap()),
            Some("a/b/c".to_string())
        );
    }

    #[test]
    fn rest_capture_has_no_static_child_and_lowest_precedence() {
        let rest = Pattern::parse("/ipfs/{cid}/{*path}").unwrap();
        let bare = Pattern::parse("/ipfs/{cid}/{leaf}").unwrap();
        let prefix = Pattern::parse("/ipfs/{cid}/v{version}").unwrap();
        let exact = Pattern::parse("/ipfs/{cid}/versions").unwrap();

        assert!(rest.static_child().is_none());
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
