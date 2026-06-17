//! Router unit tests for the object model contracts.

use super::*;
use crate::browse::FileContent;
use crate::captures::{Capture, Captures, FromCaptures};
use crate::cx::Cx;
use crate::error::{ProviderError, ProviderErrorKind, Result};
use crate::handler::DirCx;
use crate::identity::{Facet, IdentityCaptures, LogicalId};
use crate::object::{Canonical, Key, Load, Object, ObjectKind, ObjectShape};
use crate::projection::{DirProjection, Entry, FileProjection};
use crate::repr::{Markdown, Representable};
use omnifs_core::ContentType;
use omnifs_wit::provider::types as wit_types;
use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;
use std::str::FromStr;
use std::task::{Context, Poll, Waker};

// ---------------------------------------------------------------------------
// Test object + key types
// ---------------------------------------------------------------------------

#[derive(serde::Serialize, serde::Deserialize)]
struct DemoObj {
    title: String,
}

impl Object for DemoObj {
    type Key = DemoKey;

    fn kind() -> ObjectKind {
        ObjectKind("demo.obj")
    }

    fn canonical_content_type() -> ContentType {
        ContentType::Json
    }
}

impl Representable<Markdown> for DemoObj {
    fn represent(&self) -> Vec<u8> {
        format!("# {}", self.title).into_bytes()
    }
}

impl DemoObj {
    fn title(&self) -> Result<FileContent> {
        if self.title.is_empty() {
            return Err(ProviderError::invalid_input("missing title"));
        }
        Ok(FileContent::new(self.title.clone()))
    }

    fn body(&self) -> Result<FileContent> {
        if self.title.is_empty() {
            return Err(ProviderError::invalid_input("missing title"));
        }
        Ok(FileContent::new(format!("body: {}", self.title)))
    }

    fn state(&self) -> Result<FileContent> {
        if self.title.is_empty() {
            return Err(ProviderError::invalid_input("missing title"));
        }
        Ok(FileContent::new("open"))
    }
}

struct DemoKey {
    id: String,
}

impl FromCaptures for DemoKey {
    fn from_captures(caps: &Captures) -> Result<Self> {
        Ok(Self {
            id: caps
                .get("id")
                .ok_or_else(|| ProviderError::invalid_input("missing id"))?
                .to_string(),
        })
    }
}

impl IdentityCaptures for DemoKey {
    fn identity_captures(&self) -> Vec<(&'static str, String)> {
        vec![("id", self.id.clone())]
    }
}

impl crate::object::FacetMetadata for DemoKey {
    fn facet_axes() -> &'static [crate::object::FacetAxis] {
        &[]
    }
}

impl Key for DemoKey {
    type Object = DemoObj;
    type State = ();

    async fn load(
        &self,
        _cx: &Cx<Self::State>,
        _since: Option<crate::object::Validator>,
    ) -> Result<Load<Self::Object>> {
        Ok(Load::fresh_from(
            DemoObj {
                title: self.id.clone(),
            },
            Canonical {
                bytes: format!(r#"{{"title":"{}"}}"#, self.id).into_bytes(),
                validator: None,
            },
        ))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Filter {
    Open,
    All,
}

impl FromStr for Filter {
    type Err = ProviderError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "open" => Ok(Self::Open),
            "all" => Ok(Self::All),
            other => Err(ProviderError::invalid_input(format!(
                "unknown filter {other}"
            ))),
        }
    }
}

impl fmt::Display for Filter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open => write!(f, "open"),
            Self::All => write!(f, "all"),
        }
    }
}

struct FacetedKey {
    owner: String,
    number: String,
    filter: Facet<Filter>,
}

impl FromCaptures for FacetedKey {
    fn from_captures(caps: &Captures) -> Result<Self> {
        Ok(Self {
            owner: caps
                .get("owner")
                .ok_or_else(|| ProviderError::invalid_input("missing owner"))?
                .to_string(),
            number: caps
                .get("number")
                .ok_or_else(|| ProviderError::invalid_input("missing number"))?
                .to_string(),
            filter: Facet(
                caps.get("filter")
                    .ok_or_else(|| ProviderError::invalid_input("missing filter"))?
                    .parse()?,
            ),
        })
    }
}

impl IdentityCaptures for FacetedKey {
    fn identity_captures(&self) -> Vec<(&'static str, String)> {
        vec![
            ("owner", self.owner.clone()),
            ("number", self.number.clone()),
        ]
    }
}

impl crate::object::FacetMetadata for FacetedKey {
    fn facet_axes() -> &'static [crate::object::FacetAxis] {
        static AXES: [crate::object::FacetAxis; 1] = [crate::object::FacetAxis {
            capture_name: "filter",
            choices: &["open", "all"],
        }];
        &AXES
    }
}

impl FacetedKey {
    fn anchor(&self) -> LogicalId {
        LogicalId::new(ObjectKind("github.issue"), self.identity_captures())
    }
}

fn demo_handle() -> Result<ObjectHandle<DemoObj>> {
    object("/items/{id}", |o| {
        o.representations("item", (Markdown,))?;
        Ok(())
    })
}

// ---------------------------------------------------------------------------
// Contract tests (Tier-0 §8 invariants #15, #17, #18, #1 foundation)
// ---------------------------------------------------------------------------

#[test]
fn representation_dispatch() {
    let handle = demo_handle().unwrap();
    let table = &handle.spec.render_table;
    let canonical = br#"{"title":"hello"}"#;

    assert_eq!(
        table.serve(ContentType::Json, canonical).unwrap(),
        canonical.to_vec(),
        "source content type must be served verbatim"
    );

    assert_eq!(
        table.serve(ContentType::Markdown, canonical).unwrap(),
        b"# hello".as_slice(),
        "declared render must dispatch through RenderTable"
    );

    let err = table
        .serve(ContentType::Custom("text/html"), canonical)
        .unwrap_err();
    assert_eq!(err.kind(), ProviderErrorKind::NotFound);
}

#[test]
fn attach_symmetry_one_identity() {
    let handle = demo_handle().unwrap();

    let mut router = Router::<()>::new();
    router.attach("/open", &handle).unwrap();
    router.attach("/all", &handle).unwrap();
    router.seal().unwrap();

    let caps = Captures::new(vec![Capture {
        name: "id".into(),
        value: "42".into(),
    }]);
    let key = DemoKey::from_captures(&caps).unwrap();
    let id = key.anchor();

    assert_eq!(
        id,
        LogicalId::new(DemoObj::kind(), vec![("id", "42".into())]),
        "logical id must depend only on identity captures, not attach prefix"
    );
    assert!(
        id.captures
            .iter()
            .all(|(name, _)| *name != "open" && *name != "all"),
        "attach prefix must not appear in identity captures"
    );
}

#[test]
fn seal_rejects_overlapping_routes() {
    let mut router = Router::<()>::new();
    router
        .file("/items/{id}")
        .handler(|_cx: Cx<()>| async { Ok(FileProjection::inline(b"dup").build()) })
        .unwrap();
    router
        .object::<DemoObj>("/items/{id}", |o| {
            o.representations("item", (Markdown,))?;
            Ok(())
        })
        .unwrap();

    let err = router.seal().unwrap_err();
    assert_eq!(err.kind(), ProviderErrorKind::InvalidInput);
}

#[test]
fn object_listing_includes_top_level_handler_leaves_only() {
    let handle = object("/items/{id}", |o| {
        o.representations("item", (Markdown,))?;
        o.file("summary")
            .handler(|_cx: Cx<()>| async { Ok(FileProjection::inline(b"summary").build()) })?;
        o.dir("comments").handler(|_cx: DirCx<()>| async {
            Ok(DirProjection::exhaustive([Entry::file("1")]))
        })?;
        o.file("comments/{idx}")
            .handler(|_cx: Cx<()>| async { Ok(FileProjection::inline(b"comment").build()) })?;
        Ok(())
    })
    .unwrap();
    let pattern = super::pattern::Pattern::parse("/items/{id}").unwrap();

    let mounted = super::object::mount_object::<DemoObj, ()>(
        &pattern,
        ObjectShape::Dir,
        &handle.spec,
        "/items/{id}",
    )
    .unwrap();
    let leaf_names: Vec<&str> = mounted
        .entry
        .leaves
        .iter()
        .map(|leaf| leaf.name.as_str())
        .collect();

    assert_eq!(
        leaf_names,
        vec!["item.md", "item.json", "summary", "comments"]
    );
    assert_eq!(mounted.handler_files.len(), 2);
    assert_eq!(mounted.handler_dirs.len(), 1);
}

#[test]
fn projected_leaf_modifiers_apply_to_pending_leaf() {
    let handle = object("/items/{id}", |o| {
        o.representations("item", (Markdown,))?;
        o.file("title").project(DemoObj::title)?;
        o.file("body").lazy().project(DemoObj::body)?;
        o.file("state").stable().project(DemoObj::state)?;
        Ok(())
    })
    .unwrap();
    let pattern = super::pattern::Pattern::parse("/items/{id}").unwrap();
    let mounted = super::object::mount_object::<DemoObj, ()>(
        &pattern,
        ObjectShape::Dir,
        &handle.spec,
        "/items/{id}",
    )
    .unwrap();
    let cx = Cx::new(1, Rc::new(RefCell::new(())));
    let caps = Captures::new(vec![Capture {
        name: "id".into(),
        value: "42".into(),
    }]);
    let mut list = (mounted.entry.list)(&cx, caps, "/items/42".to_string());
    let waker = Waker::noop();
    let mut ctx = Context::from_waker(waker);
    let effects = match list.as_mut().poll(&mut ctx) {
        Poll::Ready(result) => result.unwrap(),
        Poll::Pending => panic!("object listing should complete without callouts"),
    };
    let mut fs: Vec<_> = effects.into_wit().fs;
    fs.sort_by(|a, b| a.path.cmp(&b.path));

    let projected: Vec<_> = fs
        .iter()
        .map(|write| match &write.kind {
            wit_types::FsKind::File(file) => (write.path.as_str(), file.attrs.stability),
            wit_types::FsKind::Directory(_) => panic!("object field preload should write files"),
        })
        .collect();

    assert_eq!(
        projected,
        vec![
            ("/items/42/state", wit_types::Stability::Stable),
            ("/items/42/title", wit_types::Stability::Dynamic),
        ],
        "lazy body must not eager-project, and stability must apply to the pending state leaf"
    );
}

#[test]
fn route_shape_tracks_object_handler_leaves() {
    let mut router = Router::<()>::new();
    router
        .object::<DemoObj>("/items/{id}", |o| {
            o.representations("item", (Markdown,))?;
            o.file("summary")
                .handler(|_cx: Cx<()>| async { Ok(FileProjection::inline(b"summary").build()) })?;
            o.dir("comments").handler(|_cx: DirCx<()>| async {
                Ok(DirProjection::exhaustive([Entry::file("1")]))
            })?;
            o.file("comments/{idx}")
                .handler(|_cx: Cx<()>| async { Ok(FileProjection::inline(b"comment").build()) })?;
            Ok(())
        })
        .unwrap();

    let shape = router.shape();
    let item = omnifs_core::path::Path::parse("/items/42").unwrap();
    let mut entries = shape.static_entries_for_parent(&item);
    entries.sort_by(|a, b| a.name().cmp(b.name()));

    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name(), "comments");
    assert_eq!(entries[0].kind(), crate::browse::EntryKind::Directory);
    assert_eq!(entries[1].name(), "summary");
    assert_eq!(entries[1].kind(), crate::browse::EntryKind::File);
    assert!(
        shape
            .list_dir_route(&omnifs_core::path::Path::parse("/items/42/comments").unwrap())
            .is_some(),
        "handler dir should be a list route"
    );
    assert!(
        shape
            .file_route(&omnifs_core::path::Path::parse("/items/42/comments/1").unwrap())
            .is_some(),
        "handler file should be a read route"
    );
}

#[test]
fn facet_excluded_from_identity() {
    let open = FacetedKey {
        owner: "acme".into(),
        number: "7".into(),
        filter: Facet(Filter::Open),
    };
    let all = FacetedKey {
        owner: "acme".into(),
        number: "7".into(),
        filter: Facet(Filter::All),
    };

    assert_eq!(open.filter.0, Filter::Open);
    assert_eq!(all.filter.0, Filter::All);
    assert_ne!(open.filter, all.filter);

    assert_eq!(open.identity_captures(), all.identity_captures());
    assert_eq!(open.anchor(), all.anchor());
    assert!(
        open.identity_captures()
            .iter()
            .all(|(name, _)| *name != "filter"),
        "facet capture must be excluded from identity"
    );
}

#[test]
fn facet_view_leaves_expand_across_aliases() {
    let pattern = super::pattern::Pattern::parse("/{owner}/issues/{filter}/{number}").unwrap();
    let expansion = super::object::FacetExpansion::for_pattern::<FacetedKey>(&pattern).unwrap();

    let leaves = expansion
        .expand_view_leaves("/acme/issues/open/7/title")
        .unwrap();

    assert_eq!(
        leaves,
        vec![
            "/acme/issues/open/7/title".to_string(),
            "/acme/issues/all/7/title".to_string(),
        ],
        "canonical-store leaves must cover every finite facet alias for the same object"
    );
}
