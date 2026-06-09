//! Route registration and object mounting.

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

/// The registration + dispatch surface.
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

    pub fn dir(&mut self, template: &'static str) -> DirRoute<'_, S> {
        DirRoute {
            router: self,
            template,
        }
    }

    pub fn file(&mut self, template: &'static str) -> FileRoute<'_, S> {
        FileRoute {
            router: self,
            template,
        }
    }

    pub fn treeref(&mut self, template: &'static str) -> TreeRefRoute<'_, S> {
        TreeRefRoute {
            router: self,
            template,
        }
    }

    /// Single-attach sugar for a dir-shaped object.
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

    /// Single-attach sugar for a file-shaped object.
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

    /// Mount a detached object handle at `prefix`.
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

    /// One-path-one-id: reject overlapping leaf claims.
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

pub struct DirRoute<'r, S> {
    pub(super) router: &'r mut Router<S>,
    pub(super) template: &'static str,
}

pub struct FileRoute<'r, S> {
    pub(super) router: &'r mut Router<S>,
    pub(super) template: &'static str,
}

pub struct TreeRefRoute<'r, S> {
    pub(super) router: &'r mut Router<S>,
    pub(super) template: &'static str,
}

impl<'r, S> DirRoute<'r, S> {
    pub fn handler<Marker, H: IntoDirHandler<S, Marker>>(self, h: H) -> Result<&'r mut Router<S>> {
        self.router.dir_at(self.template, h)?;
        Ok(self.router)
    }
}

impl<'r, S> FileRoute<'r, S> {
    pub fn handler<Marker, H: IntoFileHandler<S, Marker>>(self, h: H) -> Result<&'r mut Router<S>> {
        self.router.file_at(self.template, h)?;
        Ok(self.router)
    }
}

impl<'r, S> TreeRefRoute<'r, S> {
    pub fn handler<Marker, H: IntoTreeRefHandler<S, Marker>>(
        self,
        h: H,
    ) -> Result<&'r mut Router<S>> {
        self.router.treeref_at(self.template, h)?;
        Ok(self.router)
    }
}
