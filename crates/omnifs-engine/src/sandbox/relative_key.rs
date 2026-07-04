//! Relative path-key validation for host-owned caches.

use std::path::MAIN_SEPARATOR;

/// Validate a provider-supplied key before joining it under a cache root.
///
/// Rejects absolute paths, traversal, empty components, NUL bytes, and
/// platform separators other than `/`. `is_reserved` lets each cache
/// reject its private control directories.
pub(crate) fn is_safe_relative_key(key: &str, mut is_reserved: impl FnMut(&str) -> bool) -> bool {
    if key.is_empty() || key.starts_with('/') || key.bytes().any(|b| b == 0) {
        return false;
    }

    for component in key.split('/') {
        if component.is_empty() || component == ".." || component == "." || is_reserved(component) {
            return false;
        }
        if MAIN_SEPARATOR != '/' && component.contains(MAIN_SEPARATOR) {
            return false;
        }
    }

    true
}
