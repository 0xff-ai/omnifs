//! Object route list/read serving pipeline.

use super::super::pattern::Pattern;
use super::dispatch::{
    BoxedObjectList, BoxedObjectRead, FacetExpansion, ObjectLeaf, ObjectListing, ObjectReadTarget,
    SourceLeafAttrs,
};
use super::spec::{AnchorShape, ObjectSpec};
use crate::browse::{CachedCanonical, Effects, FileContent, ReadOutcome};
use crate::captures::{Captures, FromCaptures};
use crate::cx::Cx;
use crate::error::{ProviderError, Result};
use crate::file_attrs::{FileAttrs, FileProj, ProjBytes, ReadMode, Size, Stability, VersionToken};
use crate::object::{FacetMetadata, Key, Load, Object};
use crate::repr::RenderTable;
use omnifs_core::ContentType;

/// The typed runtime side of a mounted object.
pub(super) struct ObjectRoute<O: Object> {
    pattern: Pattern,
    shape: AnchorShape,
    leaves: Vec<ObjectLeaf<O>>,
    stability: fn(&O::Key) -> Stability,
    render_table: RenderTable,
    has_canonical: bool,
    facet_expansion: FacetExpansion,
    when: Option<fn(&O::Key) -> bool>,
}

impl<O: Object> Clone for ObjectRoute<O> {
    fn clone(&self) -> Self {
        Self {
            pattern: self.pattern.clone(),
            shape: self.shape,
            leaves: self.leaves.clone(),
            stability: self.stability,
            render_table: self.render_table.clone(),
            has_canonical: self.has_canonical,
            facet_expansion: self.facet_expansion.clone(),
            when: self.when,
        }
    }
}

impl<O: Object + 'static> ObjectRoute<O>
where
    O::Key: Key + FacetMetadata + 'static,
{
    pub(super) fn for_mount(spec: &ObjectSpec<O>, pattern: &Pattern) -> Result<Self> {
        Ok(Self {
            pattern: pattern.clone(),
            shape: spec.shape,
            leaves: spec.leaves.clone(),
            stability: spec.stability,
            render_table: spec.render_table.clone(),
            has_canonical: spec.has_canonical,
            facet_expansion: FacetExpansion::for_pattern::<O::Key>(pattern)?,
            when: spec.when,
        })
    }

    pub(super) fn read_handler(self) -> BoxedObjectRead<O::State>
    where
        O::State: 'static,
    {
        Box::new(
            move |cx: &Cx<O::State>,
                  caps: Captures,
                  target: ObjectReadTarget,
                  cached: Option<CachedCanonical>,
                  read_path: String| {
                let route = self.clone();
                Box::pin(async move { route.read(cx, caps, target, cached, read_path).await })
            },
        )
    }

    pub(super) fn list_handler(self) -> BoxedObjectList<O::State>
    where
        O::State: 'static,
    {
        Box::new(
            move |cx: &Cx<O::State>, caps: Captures, list_path: String| {
                let route = self.clone();
                Box::pin(async move { route.list(cx, caps, list_path).await })
            },
        )
    }

    /// The anchor-listing side effects: load the object and emit the
    /// canonical-store effect plus eager computed preloads.
    async fn list(
        &self,
        cx: &Cx<O::State>,
        caps: Captures,
        list_path: String,
    ) -> Result<ObjectListing> {
        let key = O::Key::from_captures(&caps)?;
        if self.when.is_some_and(|pred| !pred(&key)) {
            return Err(ProviderError::not_found(format!(
                "object not found: {list_path}"
            )));
        }
        if !self.has_canonical {
            // No canonical to store; the listing is purely the static leaf set.
            return Ok(ObjectListing {
                effects: Effects::new(),
                source: None,
            });
        }
        let stability = (self.stability)(&key);

        let since = cx.version().cloned();
        let (value, canonical, preloads) = match O::load(cx, &key, since).await? {
            Load::Fresh {
                value,
                canonical,
                preloads,
            } => (value, canonical, preloads),
            Load::Unchanged => {
                return Ok(ObjectListing {
                    effects: Effects::new(),
                    source: None,
                });
            },
            Load::NotFound => {
                return Err(ProviderError::not_found(format!(
                    "object not found: {list_path}"
                )));
            },
        };
        let source = SourceLeafAttrs {
            len: canonical.bytes.len() as u64,
            validator: canonical.validator.clone(),
            stability,
        };
        let id = key.anchor(O::kind());
        let mut effects = Effects::new();
        effects.canonical_store(
            &id,
            canonical.validator.clone(),
            canonical.bytes,
            self.view_leaves_for_base(&list_path)?,
        );
        self.project_eager_fields(&mut effects, &id, &value, &key, &list_path, stability)?;
        self.lower_preloads(&mut effects, preloads, &list_path, stability)?;
        Ok(ObjectListing {
            effects,
            source: Some(source),
        })
    }

    /// The object read path (warm, fresh, unchanged, not-found).
    async fn read(
        &self,
        cx: &Cx<O::State>,
        caps: Captures,
        target: ObjectReadTarget,
        cached: Option<CachedCanonical>,
        read_path: String,
    ) -> Result<ReadOutcome> {
        let key = O::Key::from_captures(&caps)?;
        if self.when.is_some_and(|pred| !pred(&key)) {
            return Ok(ReadOutcome::NotFound(None));
        }
        let stability = (self.stability)(&key);

        if let Some(ref push) = cached
            && push.matches_anchor(&key.anchor(O::kind()))
        {
            return ServeCtx {
                render_table: &self.render_table,
                leaves: &self.leaves,
                stability,
            }
            .serve_warm(&key, target, &push.bytes, push.validator.clone());
        }

        let since = cached.as_ref().and_then(|p| p.validator.clone());
        let (value, canonical, preloads) = match O::load(cx, &key, since).await? {
            Load::Fresh {
                value,
                canonical,
                preloads,
            } => (value, canonical, preloads),
            Load::Unchanged => {
                let bytes = cached.as_ref().map(|p| p.bytes.as_slice()).ok_or_else(|| {
                    ProviderError::internal(
                        "Load::Unchanged returned without a host-pushed canonical",
                    )
                })?;
                let validator = cached.as_ref().and_then(|p| p.validator.clone());
                return ServeCtx {
                    render_table: &self.render_table,
                    leaves: &self.leaves,
                    stability,
                }
                .serve_warm(&key, target, bytes, validator);
            },
            Load::NotFound => return Ok(ReadOutcome::NotFound(Some(key.anchor(O::kind())))),
        };
        let id = key.anchor(O::kind());
        // The anchor base of the requested object is the path its canonical is
        // anchored at. For a dir-shaped anchor the read path is a leaf under
        // the anchor, so strip the leaf; for a file-shaped anchor the read
        // path IS the anchor (no leaf to strip). Both the stored view leaves
        // and the preloads' sibling paths are computed relative to this base.
        let anchor_base = match self.shape {
            AnchorShape::File => read_path.clone(),
            AnchorShape::Dir => read_path
                .rsplit_once('/')
                .map_or_else(|| read_path.clone(), |(base, _)| base.to_string()),
        };
        let mut effects = Effects::new();
        effects.canonical_store(
            &id,
            canonical.validator.clone(),
            canonical.bytes.clone(),
            // Store every canonical-view leaf, not just the requested one, so a
            // later warm read of a sibling representation hits the view cache
            // (consistent with the anchor-listing path).
            self.view_leaves_for_base(&anchor_base)?,
        );
        // For a file-shaped anchor, the just-read file must also appear in its
        // parent directory's listing (the day you read shows up in `ls`),
        // symmetric with how preloaded siblings are projected.
        if self.shape == AnchorShape::File {
            let mut file = FileProj::deferred(
                Size::Exact(canonical.bytes.len() as u64),
                ReadMode::Full,
                stability,
            );
            if let Some(v) = &canonical.validator {
                file = file.with_version(v.clone());
            }
            effects.project_file_with_id(&anchor_base, Some(&id), file)?;
            if let Some((parent, _)) = anchor_base.rsplit_once('/')
                && !parent.is_empty()
            {
                effects.project_dir(parent)?;
            }
        }
        self.lower_preloads(&mut effects, preloads, &anchor_base, stability)?;
        ServeCtx {
            render_table: &self.render_table,
            leaves: &self.leaves,
            stability,
        }
        .serve_fresh(
            &value,
            &key,
            target,
            &canonical.bytes,
            canonical.validator.clone(),
            effects,
        )
    }

    /// Every full path that maps to this object's canonical bytes, with the
    /// anchor at `base`: each canonical-view leaf under the anchor, multiplied
    /// across facet choices. For a file-shaped anchor `base` IS the single
    /// canonical-view file (there is no leaf to append).
    fn view_leaves_for_base(&self, base: &str) -> Result<Vec<String>> {
        if self.shape == AnchorShape::File {
            return self.facet_expansion.expand_view_leaves(base);
        }
        let mut view_leaves = Vec::new();
        for leaf in &self.leaves {
            if !leaf.is_canonical_view() {
                continue;
            }
            let leaf_path = format!("{base}/{}", leaf.leaf_name());
            view_leaves.extend(self.facet_expansion.expand_view_leaves(&leaf_path)?);
        }
        Ok(view_leaves)
    }

    fn project_eager_fields(
        &self,
        effects: &mut Effects,
        id: &crate::identity::LogicalId,
        value: &O,
        key: &O::Key,
        list_path: &str,
        stability: Stability,
    ) -> Result<()> {
        for leaf in &self.leaves {
            let ObjectLeaf::Computed {
                leaf_name,
                computed,
                lazy,
            } = leaf
            else {
                continue;
            };
            if *lazy {
                continue;
            }
            let projection = computed(value, key)?;
            let Some(mut file) = projection
                .as_file_proj()
                .filter(|f| matches!(f.bytes, ProjBytes::Inline(_)))
            else {
                return Err(ProviderError::internal(format!(
                    "computed object leaf {leaf_name:?} cannot preload non-inline bytes"
                )));
            };
            file.attrs = FileAttrs::new(file.attrs.size, stability);
            effects.project_file_with_id(format!("{list_path}/{leaf_name}"), Some(id), file)?;
        }
        Ok(())
    }

    /// Lower the typed [`crate::object::Preloads`] from a fresh load onto the
    /// effects channel (R5):
    ///
    /// - `objects` (same-type siblings): store the sibling canonical against
    ///   its own anchor id, with view leaves computed from THIS object's
    ///   canonical-view faces (and facets) at the sibling's path. The sibling
    ///   path is `anchor_base` with each identity capture substituted to the
    ///   sibling's value.
    /// - `files`: `project_file`, accepting only inline/deferred sources
    ///   (`Body`/`Ranged`/`Blob` are a build error, "serve through its own
    ///   face").
    fn lower_preloads(
        &self,
        effects: &mut Effects,
        preloads: crate::object::Preloads,
        anchor_base: &str,
        stability: Stability,
    ) -> Result<()> {
        let (objects, files) = preloads.into_parts();

        // A preloaded sibling resolves on lookup, but it must also appear in its
        // parent directory's listing so `ls` of the fetched range shows it (the
        // SDK derives the directory effect from the preload path; spec Part 5).
        let mut parent_dirs: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

        for sibling in objects {
            let sibling_base =
                self.substitute_identity_captures(anchor_base, &sibling.identity_captures);
            let validator = sibling.canonical.validator.clone();
            let bytes_len = sibling.canonical.bytes.len() as u64;
            let id = crate::identity::LogicalId::new(O::kind(), sibling.identity_captures);
            let view_leaves = self.view_leaves_for_base(&sibling_base)?;
            match self.shape {
                AnchorShape::File => {
                    // A file-shaped sibling projects as a deferred dirent with an
                    // honest size, listed in its parent dir; reads serve from the
                    // stored canonical.
                    let mut file =
                        FileProj::deferred(Size::Exact(bytes_len), ReadMode::Full, stability);
                    if let Some(v) = &validator {
                        file = file.with_version(v.clone());
                    }
                    effects.project_file_with_id(&sibling_base, Some(&id), file)?;
                    if let Some((parent, _)) = sibling_base.rsplit_once('/')
                        && !parent.is_empty()
                    {
                        parent_dirs.insert(parent.to_string());
                    }
                },
                AnchorShape::Dir => {
                    parent_dirs.insert(sibling_base.clone());
                },
            }
            effects.canonical_store(&id, validator, sibling.canonical.bytes, view_leaves);
        }

        for (path, file) in files {
            let proj = file.as_file_proj().ok_or_else(|| {
                ProviderError::invalid_input(format!(
                    "preload file {path:?} has a Body/Ranged/Blob source; serve it through its own face"
                ))
            })?;
            if let Some((parent, _)) = path.rsplit_once('/')
                && !parent.is_empty()
            {
                parent_dirs.insert(parent.to_string());
            }
            effects.project_file(&path, proj)?;
        }

        for dir in parent_dirs {
            effects.project_dir(&dir)?;
        }
        Ok(())
    }

    /// Substitute each identity capture into the anchor path at that capture's
    /// segment location, yielding the sibling object's anchor base path.
    fn substitute_identity_captures(
        &self,
        anchor_base: &str,
        captures: &[(&'static str, String)],
    ) -> String {
        let offset = usize::from(anchor_base.starts_with('/'));
        let mut segments = anchor_base
            .split('/')
            .map(str::to_string)
            .collect::<Vec<_>>();
        for (name, value) in captures {
            let Some(location) = self.pattern.capture_location(name) else {
                continue;
            };
            let idx = location.segment_index() + offset;
            if let Some(segment) = segments.get_mut(idx) {
                *segment = location.render_segment(value);
            }
        }
        segments.join("/")
    }
}
// ===========================================================================
// Serve helpers
// ===========================================================================

struct ServeCtx<'a, O: Object> {
    render_table: &'a RenderTable,
    leaves: &'a [ObjectLeaf<O>],
    stability: Stability,
}

impl<O: Object> Clone for ServeCtx<'_, O> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<O: Object> Copy for ServeCtx<'_, O> {}

impl<O: Object> ServeCtx<'_, O> {
    fn serve_warm(
        self,
        key: &O::Key,
        target: ObjectReadTarget,
        bytes: &[u8],
        validator: Option<VersionToken>,
    ) -> Result<ReadOutcome> {
        self.serve_from_canonical(key, target, bytes, validator, Effects::new())
    }

    fn serve_fresh(
        self,
        value: &O,
        key: &O::Key,
        target: ObjectReadTarget,
        bytes: &[u8],
        validator: Option<VersionToken>,
        effects: Effects,
    ) -> Result<ReadOutcome> {
        match target {
            ObjectReadTarget::Computed(name) => self.serve_computed(value, key, &name, effects),
            other => self.serve_from_canonical(key, other, bytes, validator, effects),
        }
    }

    fn serve_from_canonical(
        self,
        key: &O::Key,
        target: ObjectReadTarget,
        bytes: &[u8],
        validator: Option<VersionToken>,
        effects: Effects,
    ) -> Result<ReadOutcome> {
        match target {
            ObjectReadTarget::Canonical => Ok(ReadOutcome::Found(
                FileContent::canonical(representation_attrs(
                    Size::Unknown,
                    self.stability,
                    validator,
                ))
                .with_effects(effects),
            )),
            ObjectReadTarget::Representation(ct) => {
                if ct == self.render_table.source_ct {
                    return Ok(ReadOutcome::Found(
                        FileContent::canonical(representation_attrs(
                            Size::Unknown,
                            self.stability,
                            validator,
                        ))
                        .with_effects(effects),
                    ));
                }
                let rendered = self.render_table.serve(ct, bytes)?;
                Ok(ReadOutcome::Found(
                    body_file_content(rendered, ct, self.stability, validator)
                        .with_effects(effects),
                ))
            },
            ObjectReadTarget::Computed(name) => {
                let value = O::decode(bytes)?;
                self.serve_computed(&value, key, &name, effects)
            },
            ObjectReadTarget::Direct(name) | ObjectReadTarget::Stream(name) => {
                Err(ProviderError::internal(format!(
                    "face {name:?} must be served through its own handler, not canonical bytes"
                )))
            },
        }
    }

    fn serve_computed(
        self,
        value: &O,
        key: &O::Key,
        name: &str,
        effects: Effects,
    ) -> Result<ReadOutcome> {
        for leaf in self.leaves {
            if let ObjectLeaf::Computed {
                leaf_name,
                computed,
                ..
            } = leaf
                && leaf_name == name
            {
                let content = computed(value, key)?.to_browse_content()?;
                let size = content_size(&content);
                let content = content.with_attrs(FileAttrs::new(Size::Exact(size), self.stability));
                return Ok(ReadOutcome::Found(content.with_effects(effects)));
            }
        }
        Err(ProviderError::not_found(format!("field {name} not found")))
    }
}

// ===========================================================================
// Small lowering helpers
// ===========================================================================

fn content_size(content: &FileContent) -> u64 {
    content
        .content()
        .map_or(0, |b| u64::try_from(b.len()).unwrap_or(u64::MAX))
}

fn representation_attrs(
    size: Size,
    stability: Stability,
    validator: Option<VersionToken>,
) -> FileAttrs {
    let attrs = FileAttrs::new(size, stability);
    if let Some(validator) = validator {
        attrs.with_version(validator)
    } else {
        attrs
    }
}

pub(super) fn body_file_content(
    bytes: Vec<u8>,
    ct: ContentType,
    stability: Stability,
    validator: Option<VersionToken>,
) -> FileContent {
    let size = Size::Exact(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
    FileContent::new(bytes)
        .with_attrs(representation_attrs(size, stability, validator))
        .with_content_type(ct)
}
