//! The introspectable route table.
//!
//! Every registration verb on a [`Router`](super::Router) records a
//! serializable [`RouteDescriptor`] alongside the dispatchable route, so the
//! `#[omnifs_sdk::provider]` macro glue can read the whole table back after
//! `start`/`seal` and ship it through `provider-info` at `initialize`. This is
//! the only place route shape leaves the SDK as data rather than behavior; it
//! carries no handlers, only the template, kind, and object-leaf structure a
//! host or doc tool needs to describe the surface.

use serde::{Deserialize, Serialize};

/// The kind of a registered route, mirroring the registration verbs
/// (`dir`, `file`, `treeref`, `object`, `file_object`).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RouteKind {
    Dir,
    File,
    TreeRef,
    Object,
    FileObject,
}

/// One introspected route, including (for object routes) the representation
/// leaf names and the nested file/dir children declared inside the block.
///
/// `description` is populated from a `.desc("..")` DSL call on the registering
/// builder; it is `None` when the provider left the route undescribed.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RouteDescriptor {
    /// The absolute route template, e.g. `"/items/{filter}/{number}"`.
    pub template: String,
    pub kind: RouteKind,
    /// The provider's `.desc("..")` text for this route, or `None` when
    /// undescribed. Static metadata only; carries no dispatch meaning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Object representation leaf names (`item.json`, `item.md`); empty for
    /// non-object routes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub representations: Vec<String>,
    /// Nested object file/dir leaves declared inside an object block; empty
    /// for non-object routes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<RouteDescriptor>,
}

impl RouteDescriptor {
    pub(super) fn leaf(template: String, kind: RouteKind) -> Self {
        Self {
            template,
            kind,
            description: None,
            representations: Vec::new(),
            children: Vec::new(),
        }
    }

    /// Builder-style: set the description (a no-op when `None`), returning the
    /// descriptor so leaf construction stays a single expression.
    pub(super) fn described(mut self, description: Option<String>) -> Self {
        self.description = description;
        self
    }
}

/// A provider's full ordered route table, returned by
/// [`Router::route_manifest`](super::Router::route_manifest).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct RouteManifest {
    pub routes: Vec<RouteDescriptor>,
}

impl RouteManifest {
    /// Flatten the route tree into the WIT wire encoding: a single list where
    /// each object child becomes its own entry carrying the index of its
    /// parent, every parent preceding its children (see the `route-descriptor`
    /// record docs in `provider.wit`).
    #[must_use]
    pub fn to_wit(&self) -> Vec<omnifs_wit::provider::types::RouteDescriptor> {
        let mut out = Vec::new();
        for route in &self.routes {
            push_wit(&mut out, route, None);
        }
        out
    }
}

fn push_wit(
    out: &mut Vec<omnifs_wit::provider::types::RouteDescriptor>,
    route: &RouteDescriptor,
    parent: Option<u32>,
) {
    let index = u32::try_from(out.len()).unwrap_or(u32::MAX);
    out.push(omnifs_wit::provider::types::RouteDescriptor {
        template: route.template.clone(),
        kind: route.kind.into(),
        description: route.description.clone(),
        representations: route.representations.clone(),
        parent,
    });
    for child in &route.children {
        push_wit(out, child, Some(index));
    }
}

impl From<RouteKind> for omnifs_wit::provider::types::RouteKind {
    fn from(kind: RouteKind) -> Self {
        match kind {
            RouteKind::Dir => Self::Dir,
            RouteKind::File => Self::File,
            RouteKind::TreeRef => Self::TreeRef,
            RouteKind::Object => Self::Object,
            RouteKind::FileObject => Self::FileObject,
        }
    }
}
