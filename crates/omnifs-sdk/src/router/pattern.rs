//! Path pattern parsing and match helpers.

use super::handlers::{DirEntry, FileEntry, RouteValidator, TreeRefEntry};
use crate::captures::Captures;
use crate::error::{ProviderError, Result};
use omnifs_core::path::{Path, Pattern, Segment};

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
