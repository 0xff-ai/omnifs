//! The typed invalidation builder event handlers return.
//!
//! Events (timers, webhooks) are how a provider tells the host that cached
//! state is stale: there are no TTLs. An event handler returns an
//! [`Invalidation`], which lowers to the host `invalidation` effect channel
//! (evict an object's canonical and its view leaves, or a cached listing at a
//! path/prefix). This replaces returning a raw [`crate::browse::Effects`].

use crate::browse::Effects;
use crate::object::{Key, Object};

/// A set of host invalidations to apply with an accepted event return.
#[derive(Default)]
pub struct Invalidation {
    effects: Effects,
}

impl Invalidation {
    pub fn new() -> Self {
        Self::default()
    }

    /// Evict the cached listing at exactly `path`.
    #[must_use]
    pub fn listing_path(mut self, path: impl AsRef<str>) -> Self {
        self.effects.invalidate_listing_path(path);
        self
    }

    /// Evict every cached listing under `prefix` (inclusive).
    #[must_use]
    pub fn listing_prefix(mut self, prefix: impl AsRef<str>) -> Self {
        self.effects.invalidate_listing_prefix(prefix);
        self
    }

    /// Evict an object's canonical bytes and every view leaf derived from it,
    /// keyed by the object's logical id (`key.anchor(O::kind())`).
    #[must_use]
    pub fn object<O: Object>(mut self, key: &O::Key) -> Self {
        let id = key.anchor(O::kind());
        self.effects.invalidate_object(&id);
        self
    }

    /// Lower to the host effect channel (used by the `#[provider]` event glue).
    #[doc(hidden)]
    pub fn into_effects(self) -> Effects {
        self.effects
    }
}
