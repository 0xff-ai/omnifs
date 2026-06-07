//! object model: typed canonical resources addressed by typed keys.

use crate::browse::FileContent;
use crate::captures::FromCaptures;
use crate::cx::Cx;
use crate::error::{ProviderError, Result};
use crate::file_attrs::{Stability, VersionToken};
use crate::identity::{IdentityCaptures, LogicalId};
use omnifs_core::ContentType;

/// An object kind tag for the canonical store and diagnostics (e.g.
/// `github.issue`). A compile-time constant supplied by the `#[object]` macro.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjectKind(pub &'static str);

impl ObjectKind {
    pub fn as_str(self) -> &'static str {
        self.0
    }
}

/// The conditional-request validator at the object layer.
pub type Validator = VersionToken;

/// The canonical bytes the remote returned, captured verbatim on a load.
pub struct Canonical {
    pub bytes: Vec<u8>,
    pub validator: Option<Validator>,
}

/// The outcome of a key's [`Key::load`].
pub enum Load<T> {
    Fresh { value: T, canonical: Canonical },
    Unchanged,
    NotFound,
}

impl<T> Load<T> {
    pub fn fresh(value: T) -> Self {
        Self::Fresh {
            value,
            canonical: Canonical {
                bytes: Vec::new(),
                validator: None,
            },
        }
    }
}

/// A typed canonical resource.
pub trait Object: serde::Serialize + serde::de::DeserializeOwned + Sized {
    type Key: Key<Object = Self>;

    fn kind() -> ObjectKind;

    fn canonical_content_type() -> ContentType {
        ContentType::Json
    }

    fn default_stability() -> Stability {
        Stability::Mutable
    }

    fn parse_canonical(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes)
            .map_err(|e| ProviderError::invalid_input(format!("canonical parse: {e}")))
    }
}

/// Filesystem shape of an object anchor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectShape {
    Dir,
    File,
}

/// The identity side of an object: fetch, child handlers, and logical id.
pub trait Key: FromCaptures + IdentityCaptures + Sized {
    type Object: Object<Key = Self>;
    type State;

    fn load(
        &self,
        cx: &Cx<Self::State>,
        since: Option<Validator>,
    ) -> impl core::future::Future<Output = Result<Load<Self::Object>>>;

    fn anchor(&self) -> LogicalId {
        LogicalId::new(Self::Object::kind(), self.identity_captures())
    }
}

/// Route-context capture metadata for multikey view-leaf expansion.
///
/// Emitted by `#[path_captures]` for each `Facet<T>` field whose `T: PathSegment`
/// exposes a finite `choices()` set.
pub trait FacetMetadata {
    fn facet_axes() -> &'static [FacetAxis];
}

/// A finite facet dimension: the template capture name and its segment choices.
#[derive(Clone, Copy, Debug)]
pub struct FacetAxis {
    pub capture_name: &'static str,
    pub choices: &'static [&'static str],
}

impl FacetMetadata for () {
    fn facet_axes() -> &'static [FacetAxis] {
        &[]
    }
}

/// Project a field leaf from a loaded object value.
pub type ProjectFn<O> = fn(&O) -> Result<FileContent>;
