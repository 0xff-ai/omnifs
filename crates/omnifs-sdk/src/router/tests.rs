//! Router unit tests for the object model contracts.

use super::*;
use crate::browse::FileContent;
use crate::captures::{Capture, Captures, FromCaptures};
use crate::cx::Cx;
use crate::error::{ProviderError, ProviderErrorKind, Result};
use crate::handler::DirCx;
use crate::identity::{Facet, IdentityCaptures, LogicalId};
use crate::object::{Canonical, Key, Load, Object, ObjectKind};
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
        o.dynamic();
        Ok(())
    })
}

#[test]
fn object_without_stability_fails_to_finish() {
    let result = object::<DemoObj>("/items/{id}", |o| {
        o.representations("item", (Markdown,))?;
        Ok(())
    });
    let Err(err) = result else {
        panic!("an object block that declares no stability must fail to finish");
    };
    assert_eq!(err.kind(), ProviderErrorKind::InvalidInput);
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
            o.dynamic();
            Ok(())
        })
        .unwrap();

    let err = router.seal().unwrap_err();
    assert_eq!(err.kind(), ProviderErrorKind::InvalidInput);
}

#[test]
fn object_mount_lists_only_canonical_derived_leaves() {
    let handle = object("/items/{id}", |o| {
        o.representations("item", (Markdown,))?;
        o.dynamic();
        o.file("summary")
            .project(|value: &DemoObj, _key| value.title())?;
        Ok(())
    })
    .unwrap();
    let pattern = super::pattern::Pattern::parse("/items/{id}").unwrap();

    let mounted =
        super::object::mount_object::<DemoObj, ()>(&pattern, &handle.spec, "/items/{id}").unwrap();
    let leaf_names: Vec<&str> = mounted
        .entry
        .leaves
        .iter()
        .map(|leaf| leaf.name.as_str())
        .collect();

    assert_eq!(leaf_names, vec!["item.md", "item.json", "summary"]);
}

#[test]
fn lazy_excluded_eager_leaves_inherit_object_stability() {
    let handle = object("/items/{id}", |o| {
        o.representations("item", (Markdown,))?;
        o.dynamic();
        o.file("title")
            .project(|value: &DemoObj, _key| value.title())?;
        o.file("body")
            .lazy()
            .project(|value: &DemoObj, _key| value.body())?;
        o.file("state")
            .project(|value: &DemoObj, _key| value.state())?;
        Ok(())
    })
    .unwrap();
    let pattern = super::pattern::Pattern::parse("/items/{id}").unwrap();
    let mounted =
        super::object::mount_object::<DemoObj, ()>(&pattern, &handle.spec, "/items/{id}").unwrap();
    let cx = Cx::new(1, Rc::new(RefCell::new(())));
    let caps = Captures::new(vec![Capture {
        name: "id".into(),
        value: "42".into(),
    }]);
    let mut list = (mounted.entry.list)(&cx, caps, "/items/42".to_string());
    let waker = Waker::noop();
    let mut ctx = Context::from_waker(waker);
    let listing = match list.as_mut().poll(&mut ctx) {
        Poll::Ready(result) => result.unwrap(),
        Poll::Pending => panic!("object listing should complete without callouts"),
    };
    let mut fs: Vec<_> = listing.effects.into_wit().fs;
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
            ("/items/42/state", wit_types::Stability::Dynamic),
            ("/items/42/title", wit_types::Stability::Dynamic),
        ],
        "lazy body must not eager-project; every eager leaf carries the object's stability"
    );
}

#[test]
fn route_shape_tracks_explicit_child_routes_under_object_anchor() {
    let mut router = Router::<()>::new();
    router
        .object::<DemoObj>("/items/{id}", |o| {
            o.representations("item", (Markdown,))?;
            o.dynamic();
            Ok(())
        })
        .unwrap();
    router
        .file("/items/{id}/summary")
        .handler(|_cx: Cx<()>| async { Ok(FileProjection::inline(b"summary").build()) })
        .unwrap();
    router
        .dir("/items/{id}/comments")
        .handler(|_cx: DirCx<()>| async { Ok(DirProjection::exhaustive([Entry::file("1")])) })
        .unwrap();
    router
        .file("/items/{id}/comments/{idx}")
        .handler(|_cx: Cx<()>| async { Ok(FileProjection::inline(b"comment").build()) })
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
        "explicit child dir should be a list route"
    );
    assert!(
        shape
            .file_route(&omnifs_core::path::Path::parse("/items/42/comments/1").unwrap())
            .is_some(),
        "explicit child file should be a read route"
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

/// Regression: looking up one child of an object directory must report the
/// anchor's OTHER leaves as siblings.
///
/// An object-dir child lookup answers `exhaustive` (the leaf set is statically
/// known, so absence is authoritative). The host folds an exhaustive lookup's
/// `target + siblings` into the parent's cached dirents as the whole directory.
/// If the lookup omitted its siblings, that fold would shrink the dirents to the
/// single looked-up child, so a later `readdir` would enumerate only that child
/// (reading `body` makes `ls` of the issue dir return just `body`). Asserting
/// the full sibling set here keeps the lookup honest at the source.
#[test]
fn object_dir_child_lookup_carries_all_sibling_leaves() {
    let mut router = Router::<()>::new();
    router
        .object::<DemoObj>("/items/{id}", |o| {
            o.representations("item", (Markdown,))?;
            o.dynamic();
            o.file("title")
                .project(|value: &DemoObj, _key| value.title())?;
            o.file("body")
                .lazy()
                .project(|value: &DemoObj, _key| value.body())?;
            o.file("state")
                .project(|value: &DemoObj, _key| value.state())?;
            Ok(())
        })
        .unwrap();
    router
        .dir("/items/{id}/comments")
        .handler(|_cx: DirCx<()>| async { Ok(DirProjection::exhaustive([Entry::file("1")])) })
        .unwrap();
    router.seal().unwrap();

    let cx = Cx::new(1, Rc::new(RefCell::new(())));
    let mut fut = Box::pin(router.lookup_child(&cx, "/items/42", "body"));
    let waker = Waker::noop();
    let mut ctx = Context::from_waker(waker);
    let lookup = match fut.as_mut().poll(&mut ctx) {
        Poll::Ready(result) => result.unwrap(),
        Poll::Pending => panic!("an object-dir leaf lookup resolves without callouts"),
    };

    let (wire, _effects) = lookup.into_result_and_effects();
    let wit_types::LookupChildResult::Entry(entry) = wire else {
        panic!("object-dir leaf lookup must resolve to an entry");
    };

    assert_eq!(
        entry.target.name, "body",
        "the looked-up leaf is the target"
    );
    assert!(
        entry.exhaustive,
        "an object's statically-known leaf set is exhaustive"
    );

    let mut sibling_names: Vec<&str> = entry.siblings.iter().map(|s| s.name.as_str()).collect();
    sibling_names.sort_unstable();
    assert_eq!(
        sibling_names,
        vec!["comments", "item.json", "item.md", "state", "title"],
        "an exhaustive object-dir leaf lookup must carry every other leaf as a \
         sibling, or the host's lookup-hints fold collapses the directory to the \
         single looked-up child"
    );
}

#[test]
fn dynamic_capture_prefix_lists_route_table_children_without_stub_dir() {
    let mut router = Router::<()>::new();
    router
        .file("/items/{id}/body")
        .handler(|_cx: Cx<()>| async { Ok(FileProjection::body(b"body".to_vec()).build()) })
        .unwrap();
    router.seal().unwrap();

    let cx = Cx::new(1, Rc::new(RefCell::new(())));

    let mut lookup = Box::pin(router.lookup_child(&cx, "/items", "42"));
    let waker = Waker::noop();
    let mut ctx = Context::from_waker(waker);
    let lookup = match lookup.as_mut().poll(&mut ctx) {
        Poll::Ready(result) => result.unwrap(),
        Poll::Pending => panic!("implicit dynamic directory lookup resolves without callouts"),
    };
    let (wire, _effects) = lookup.into_result_and_effects();
    let wit_types::LookupChildResult::Entry(entry) = wire else {
        panic!("dynamic capture prefix should resolve as a directory");
    };
    assert_eq!(entry.target.name, "42");
    assert!(matches!(entry.target.kind, wit_types::EntryKind::Directory));

    let mut list = Box::pin(router.list_children(&cx, "/items/42", None, None));
    let listing = match list.as_mut().poll(&mut ctx) {
        Poll::Ready(result) => result.unwrap(),
        Poll::Pending => panic!("implicit dynamic directory listing resolves without callouts"),
    };
    let (wire, _effects) = listing.into_result_and_effects();
    let wit_types::ListChildrenResult::Entries(listing) = wire else {
        panic!("dynamic capture prefix should list static route-table children");
    };
    let names: Vec<&str> = listing
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert_eq!(names, vec!["body"]);
}
