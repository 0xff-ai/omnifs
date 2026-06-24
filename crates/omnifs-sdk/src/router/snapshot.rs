//! A route snapshot: the readable mount tree plus the seal verdict.
//!
//! [`RouteSnapshot`] is what `Provider::routes()` (emitted by
//! `#[omnifs_sdk::provider]`) returns, so a provider's whole path surface is
//! visible without booting the host. [`Self::assert_valid`] runs the seal
//! checks and panics on the first error, which is how the `routes_are_valid()`
//! snapshot test replaces provider unit tests that pinned helper names.

use super::pattern::Pattern;
use super::register::Router;
use crate::error::ProviderError;
use std::fmt;

/// A captured view of a router's mount: the route templates (for the
/// [`fmt::Display`] tree) and the seal verdict.
pub struct RouteSnapshot {
    templates: Vec<String>,
    seal: Result<(), ProviderError>,
}

impl RouteSnapshot {
    /// Snapshot a sealed router: collect its leaf templates and run the seal
    /// checks. Tolerant of a failed seal (stored, not panicked) so a snapshot
    /// can render even an invalid mount.
    pub fn capture<S: 'static>(router: &mut Router<S>) -> Self {
        let seal = router.seal();
        let mut templates: Vec<String> = router
            .leaf_claims
            .iter()
            .map(Pattern::parent_signature)
            .collect();
        templates.sort();
        templates.dedup();
        Self { templates, seal }
    }

    /// A snapshot for a provider whose `start()` failed to build a router: no
    /// routes, the start error as the seal verdict.
    pub fn start_error(error: ProviderError) -> Self {
        Self {
            templates: Vec::new(),
            seal: Err(error),
        }
    }

    /// Whether the mount sealed cleanly.
    pub fn is_valid(&self) -> bool {
        self.seal.is_ok()
    }

    /// Run the seal checks; panic with the first error. Used by the
    /// `routes_are_valid()` snapshot test.
    pub fn assert_valid(&self) {
        if let Err(error) = &self.seal {
            panic!("route snapshot is not valid: {error}");
        }
    }
}

impl fmt::Display for RouteSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // An indented tree: split each template into segments and indent by
        // depth, deduplicating shared prefixes.
        let mut last: Vec<&str> = Vec::new();
        for template in &self.templates {
            let segments: Vec<&str> = template.trim_start_matches('/').split('/').collect();
            let shared = last
                .iter()
                .zip(&segments)
                .take_while(|(a, b)| a == b)
                .count();
            for (depth, segment) in segments.iter().enumerate().skip(shared) {
                writeln!(f, "{}{segment}", "  ".repeat(depth))?;
            }
            last = segments;
        }
        if let Err(error) = &self.seal {
            writeln!(f, "[invalid: {error}]")?;
        }
        Ok(())
    }
}
