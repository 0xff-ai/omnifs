//! `read_file` and `open_file` dispatch.

use crate::browse::{CachedCanonical, ReadOutcome};
use crate::cx::Cx;
use crate::error::{ProviderError, Result};
use crate::handler::OpenedFile;
use crate::projection::FileSource;

use super::super::pattern::parse_provider_path;
use super::super::register::Router;
use super::route_shape::ReadRoute;

impl<S> Router<S> {
    /// Read the bytes at an absolute file path.
    ///
    /// A plain file route (including object handler files) wins first: its
    /// handler runs and the projection lowers to a one-shot content
    /// terminal. Otherwise the path resolves through the object read path:
    /// either the path is an object anchor (`content_type` selects the
    /// representation, falling back to octet-stream when the mime string is
    /// unknown) or a leaf under a dir-shaped anchor (representation by
    /// `stem.ext` name, projected field by name).
    ///
    /// `cached` is the host-pushed canonical for that object id; the object
    /// path uses it to re-render without an upstream call. Plain file routes
    /// ignore it: they have no canonical identity.
    ///
    /// Projection limits on this path: a `Deferred` source cannot answer the
    /// read it deferred to (the handler must return real bytes here), and
    /// `Ranged` sources are served through `open_file`/`read_chunk`, not
    /// `read_file`.
    pub async fn read_file(
        &self,
        cx: &Cx<S>,
        path: &str,
        content_type: &str,
        cached: Option<CachedCanonical>,
    ) -> Result<ReadOutcome> {
        debug_assert!(path.starts_with('/'), "read_file expects an absolute path");
        let abs = parse_provider_path(path)?;
        let shape = self.shape();

        match shape.read_route(&abs, content_type) {
            Some(ReadRoute::File(route)) => {
                let proj = (route.entry.handler)(cx.clone(), route.captures).await?;
                proj.into_browse_content().map(ReadOutcome::Found)
            },
            Some(ReadRoute::Object { route, target }) => {
                (route.entry.read)(cx, route.captures, target, cached, path.to_string()).await
            },
            None => Err(ProviderError::not_found(format!("path not found: {path}"))),
        }
    }

    /// Open a ranged read session at an absolute file path.
    ///
    /// Only file routes participate, including object handler file leaves
    /// (representation and projected leaves are whole-byte reads), and the
    /// handler must return a [`FileSource::Ranged`] projection; anything
    /// else is an input error.
    /// The returned reader serves subsequent `read_chunk` calls.
    pub async fn open_file(&self, cx: &Cx<S>, path: &str) -> Result<OpenedFile> {
        debug_assert!(path.starts_with('/'), "open_file expects an absolute path");
        let abs = parse_provider_path(path)?;
        let shape = self.shape();

        if let Some(route) = shape.file_route(&abs) {
            let proj = (route.entry.handler)(cx.clone(), route.captures).await?;
            return match proj.source() {
                FileSource::Ranged(reader) => {
                    Ok(OpenedFile::new(proj.attrs().clone(), reader.clone()))
                },
                _ => Err(ProviderError::invalid_input(format!(
                    "open_file requires a ranged projection; path {path:?} returned a non-ranged source"
                ))),
            };
        }

        Err(ProviderError::not_found(format!("path not found: {path}")))
    }
}
