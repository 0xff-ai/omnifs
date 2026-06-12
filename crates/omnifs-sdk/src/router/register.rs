//! Route registration and object mounting.
//!
//! Everything here runs once, inside a provider's `start`. The route table is
//! append-only and immutable after [`Router::seal`]; dispatch (the
//! `dispatch` submodule) reads it on every host browse call.

use crate::error::{ProviderError, Result};
use crate::object::Object;

use super::handlers::{IntoDirHandler, IntoFileHandler, IntoTreeRefHandler};
use super::object::{
    DirObjectBlock, FileObjectBlock, ObjectHandle, file_object, mount_object, object,
};
use super::pattern::parse_pattern;

// ===========================================================================
// Router
// ===========================================================================

/// The registration and dispatch surface; `S` is the provider state type
/// handlers receive through their `Cx<S>` / `DirCx<S>`.
///
/// Routes live in per-kind tables (dirs, files, treerefs, objects, plus the
/// file/dir handler leaves objects contribute). Separately,
/// `leaf_claims` accumulates one pattern per leaf so [`Self::seal`] can
/// enforce one-path-one-route across all kinds at once.
///
/// ```ignore
/// fn start(config: Config, r: &mut Router<State>) -> Result<State> {
///     r.dir("/").handler(root_list)?;
///     r.file("/rate_limit").handler(read_rate_limit)?;   // literal beats {owner}
///     r.dir("/{owner}").handler(OwnerKey::repos)?;
///     r.treeref("/{owner}/{repo}/repo").handler(RepoKey::tree)?;
///     r.object::<Issue>("/{owner}/{repo}/issues/{filter}/{number}", |o| {
///         o.representations("item", (Markdown,))?;
///         o.file("title").project(Issue::title)?;
///         o.file("body").lazy().project(Issue::body)?;
///         Ok(())
///     })?;
///     Ok(State::default())
/// }
/// ```
pub struct Router<S = ()> {
    pub(super) dirs: Vec<super::handlers::DirEntry<S>>,
    pub(super) files: Vec<super::handlers::FileEntry<S>>,
    pub(super) treerefs: Vec<super::handlers::TreeRefEntry<S>>,
    pub(super) objects: Vec<super::object::ObjectEntry<S>>,
    pub(super) handler_files: Vec<super::handlers::FileEntry<S>>,
    pub(super) handler_dirs: Vec<super::handlers::DirEntry<S>>,
    pub(super) leaf_claims: Vec<super::pattern::Pattern>,
}

impl<S> Default for Router<S> {
    fn default() -> Self {
        Self {
            dirs: Vec::new(),
            files: Vec::new(),
            treerefs: Vec::new(),
            objects: Vec::new(),
            handler_files: Vec::new(),
            handler_dirs: Vec::new(),
            leaf_claims: Vec::new(),
        }
    }
}

impl<S> Router<S> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Begin a directory route at `template`; finish with
    /// [`DirRoute::handler`]. The handler answers both listing and
    /// child-lookup intents (see [`crate::handler::DirIntent`]) and returns a
    /// [`crate::projection::DirProjection`].
    pub fn dir(&mut self, template: &'static str) -> DirRoute<'_, S> {
        DirRoute {
            router: self,
            template,
        }
    }

    /// Begin a file route at `template`; finish with [`FileRoute::handler`].
    /// The handler returns a [`crate::projection::FileProjection`].
    pub fn file(&mut self, template: &'static str) -> FileRoute<'_, S> {
        FileRoute {
            router: self,
            template,
        }
    }

    /// Begin a subtree-handoff route at `template`; finish with
    /// [`TreeRefRoute::handler`]. The handler returns a
    /// [`crate::handler::TreeRef`] and the host takes over the whole subtree
    /// (bind-mounted clone or archive tree); provider dispatch never sees
    /// paths below it.
    pub fn treeref(&mut self, template: &'static str) -> TreeRefRoute<'_, S> {
        TreeRefRoute {
            router: self,
            template,
        }
    }

    /// Bind a dir-shaped [`Object`] at `template`: define and attach in one
    /// call. The anchor path becomes a directory whose children are the
    /// representations and leaves declared in `block` (which must call
    /// [`DirObjectBlock::representations`]). `O::Key` is parsed from the
    /// template's captures and supplies `load`; there is no separate fetcher.
    ///
    /// Use [`object()`] plus [`Self::attach`] instead when the same object
    /// subtree must be mounted under several prefixes.
    pub fn object<O: Object + 'static>(
        &mut self,
        template: &'static str,
        block: impl FnOnce(&mut DirObjectBlock<O>) -> Result<()>,
    ) -> Result<&mut Self>
    where
        O::Key: crate::object::Key<State = S> + crate::object::FacetMetadata + 'static,
        S: 'static,
    {
        let handle = object(template, block)?;
        self.mount_handle("", &handle)
    }

    /// Bind a file-shaped [`Object`] at `template`. Like [`Self::object`] but
    /// the anchor presents as a file rather than a directory of leaves;
    /// the block only declares representations and an optional `when`
    /// predicate (no child leaves).
    pub fn file_object<O: Object + 'static>(
        &mut self,
        template: &'static str,
        block: impl FnOnce(&mut FileObjectBlock<O>) -> Result<()>,
    ) -> Result<&mut Self>
    where
        O::Key: crate::object::Key<State = S> + crate::object::FacetMetadata + 'static,
        S: 'static,
    {
        let handle = file_object(template, block)?;
        self.mount_handle("", &handle)
    }

    /// Mount a detached [`ObjectHandle`] under `prefix`. The handle's
    /// absolute template is appended to `prefix` (trailing slashes on
    /// `prefix` are trimmed), so one object definition can be replayed at
    /// multiple attach points; each attach claims its own leaves.
    pub fn attach<O: Object + 'static>(
        &mut self,
        prefix: &str,
        handle: &ObjectHandle<O>,
    ) -> Result<&mut Self>
    where
        O::Key: crate::object::Key<State = S> + crate::object::FacetMetadata + 'static,
        S: 'static,
    {
        self.mount_handle(prefix, handle)
    }

    fn mount_handle<O: Object + 'static>(
        &mut self,
        prefix: &str,
        handle: &ObjectHandle<O>,
    ) -> Result<&mut Self>
    where
        O::Key: crate::object::Key<State = S> + crate::object::FacetMetadata + 'static,
        S: 'static,
    {
        let combined = combine_template(prefix, handle.template)?;
        let pattern = parse_pattern(&combined)?;
        let mounted = mount_object(&pattern, handle.shape, handle.spec.as_ref(), &combined)?;
        self.objects.push(mounted.entry);
        self.leaf_claims.extend(mounted.claims);
        self.handler_files.extend(mounted.handler_files);
        self.handler_dirs.extend(mounted.handler_dirs);
        Ok(self)
    }

    fn dir_at<Marker, H: IntoDirHandler<S, Marker>>(&mut self, template: &str, h: H) -> Result<()> {
        let pattern = parse_pattern(template)?;
        let (handler, validator) = h.into_dir_handler();
        self.dirs.push(super::handlers::DirEntry {
            pattern: pattern.clone(),
            handler,
            validator,
        });
        self.leaf_claims.push(pattern);
        Ok(())
    }

    fn file_at<Marker, H: IntoFileHandler<S, Marker>>(
        &mut self,
        template: &str,
        h: H,
    ) -> Result<()> {
        let pattern = parse_pattern(template)?;
        let (handler, validator) = h.into_file_handler();
        self.files.push(super::handlers::FileEntry {
            pattern: pattern.clone(),
            handler,
            validator,
        });
        self.leaf_claims.push(pattern);
        Ok(())
    }

    fn treeref_at<Marker, H: IntoTreeRefHandler<S, Marker>>(
        &mut self,
        template: &str,
        h: H,
    ) -> Result<()> {
        let pattern = parse_pattern(template)?;
        let (handler, validator) = h.into_treeref_handler();
        self.treerefs.push(super::handlers::TreeRefEntry {
            pattern: pattern.clone(),
            handler,
            validator,
        });
        self.leaf_claims.push(pattern);
        Ok(())
    }

    /// One-path-one-route: reject overlapping leaf claims.
    ///
    /// Called by the `#[omnifs_sdk::provider]` macro glue after `start`
    /// returns; providers do not call it themselves. Every registration verb
    /// records the leaf patterns it claims (a route template, an object
    /// anchor plus each representation/field/handler leaf), and this check
    /// fails initialization when any pair is ambiguous under the pattern
    /// overlap rule (see the `pattern` module docs): equal-precedence
    /// patterns that can bind the same concrete path.
    /// Overlap with different precedence (a literal next to a capture at the
    /// same depth) is legal and resolves by specificity at dispatch.
    pub fn seal(&self) -> Result<()> {
        for (i, left) in self.leaf_claims.iter().enumerate() {
            for right in self.leaf_claims.iter().skip(i + 1) {
                if left.is_ambiguous_with(right) {
                    return Err(ProviderError::invalid_input(format!(
                        "overlapping routes: {} vs {}",
                        left.parent_signature(),
                        right.parent_signature()
                    )));
                }
            }
        }
        Ok(())
    }
}

/// Join an attach prefix with an object's absolute template. The template
/// must stay absolute so a handle reads identically whether attached at the
/// root (empty prefix) or under a deeper prefix.
fn combine_template(prefix: &str, template: &str) -> Result<String> {
    let prefix = prefix.trim_end_matches('/');
    let template = if template.starts_with('/') {
        template
    } else {
        return Err(ProviderError::invalid_input(format!(
            "object template must be absolute: {template}"
        )));
    };
    if prefix.is_empty() {
        return Ok(template.to_string());
    }
    Ok(format!("{prefix}{template}"))
}

/// A pending [`Router::dir`] registration; nothing is recorded until
/// [`Self::handler`] runs.
pub struct DirRoute<'r, S> {
    pub(super) router: &'r mut Router<S>,
    pub(super) template: &'static str,
}

/// A pending [`Router::file`] registration; nothing is recorded until
/// [`Self::handler`] runs.
pub struct FileRoute<'r, S> {
    pub(super) router: &'r mut Router<S>,
    pub(super) template: &'static str,
}

/// A pending [`Router::treeref`] registration; nothing is recorded until
/// [`Self::handler`] runs.
pub struct TreeRefRoute<'r, S> {
    pub(super) router: &'r mut Router<S>,
    pub(super) template: &'static str,
}

impl<'r, S> DirRoute<'r, S> {
    /// Register the directory handler and claim the template as a leaf.
    /// Accepted shapes (the `Marker` parameter disambiguates them; see
    /// [`IntoDirHandler`]): `async fn(DirCx<S>)`, `async fn(DirCx<S>, K)`,
    /// `async fn(K, DirCx<S>)`, or sync `fn(K, DirCx<S>)`, with
    /// `K: FromCaptures`. Errors only on an invalid template.
    pub fn handler<Marker, H: IntoDirHandler<S, Marker>>(self, h: H) -> Result<&'r mut Router<S>> {
        self.router.dir_at(self.template, h)?;
        Ok(self.router)
    }
}

impl<'r, S> FileRoute<'r, S> {
    /// Register the file handler and claim the template as a leaf. Accepted
    /// shapes: `async fn(Cx<S>)`, `async fn(Cx<S>, K)`, or
    /// `async fn(K, Cx<S>)`, with `K: FromCaptures`.
    pub fn handler<Marker, H: IntoFileHandler<S, Marker>>(self, h: H) -> Result<&'r mut Router<S>> {
        self.router.file_at(self.template, h)?;
        Ok(self.router)
    }
}

impl<'r, S> TreeRefRoute<'r, S> {
    /// Register the subtree-handoff handler and claim the template as a
    /// leaf. Accepted shapes mirror [`FileRoute::handler`] but return a
    /// [`crate::handler::TreeRef`].
    pub fn handler<Marker, H: IntoTreeRefHandler<S, Marker>>(
        self,
        h: H,
    ) -> Result<&'r mut Router<S>> {
        self.router.treeref_at(self.template, h)?;
        Ok(self.router)
    }
}
