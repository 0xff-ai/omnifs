//! Sealed route table introspection.

use crate::captures::CaptureDescriptor;
use serde::Serialize;

use super::pattern::Pattern;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct RouteDescriptor {
    pub template: String,
    pub kind: RouteKind,
    pub object_kind: Option<String>,
    pub captures: Vec<CaptureDescriptor>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
pub enum RouteKind {
    Dir,
    File,
    Treeref,
    Object,
    FileObject,
    Alias,
    Collection,
}

impl RouteDescriptor {
    pub(super) fn new(
        pattern: &Pattern,
        kind: RouteKind,
        object_kind: Option<String>,
        typed_captures: &[CaptureDescriptor],
    ) -> Self {
        Self {
            template: pattern.template(),
            kind,
            object_kind,
            captures: pattern.capture_descriptors(typed_captures),
        }
    }
}
