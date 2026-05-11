use omnifs_sdk::Cx;
use omnifs_sdk::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Clone)]
#[omnifs_sdk::config]
struct Config;

#[derive(Clone)]
struct State;

mod root_handlers {
    use super::*;

    pub struct RootHandlers;

    #[omnifs_sdk::handlers]
    impl RootHandlers {
        #[omnifs_sdk::dir("/")]
        async fn root() -> Result<Projection> {
            Ok(Projection::new())
        }
    }
}

mod hello_handlers {
    use super::*;

    pub struct HelloHandlers;

    #[omnifs_sdk::handlers]
    impl HelloHandlers {
        #[omnifs_sdk::dir("/hello")]
        async fn hello_dir() -> Result<Projection> {
            Ok(Projection::new())
        }

        #[omnifs_sdk::file("/hello/{name}")]
        async fn hello(name: String) -> Result<FileContent> {
            Ok(FileContent::bytes(format!("hello {name}\n")))
        }
    }
}

mod extras_handlers {
    use super::*;

    pub struct ExtrasHandlers;

    #[omnifs_sdk::handlers]
    impl ExtrasHandlers {
        #[omnifs_sdk::dir("/bundle")]
        async fn bundle() -> Result<Projection> {
            let mut projection = Projection::new();
            projection.file_with_content("title", b"bundle title\n".to_vec());
            Ok(projection)
        }

        #[omnifs_sdk::treeref("/checkout")]
        async fn checkout() -> Result<TreeRef> {
            Ok(TreeRef::new(42))
        }
    }
}

mod rest_handlers {
    use super::*;

    pub struct RestHandlers;

    #[omnifs_sdk::handlers]
    impl RestHandlers {
        #[omnifs_sdk::file("/root/{a}/{*rest}")]
        async fn rest_file(a: String, rest: String) -> Result<FileContent> {
            Ok(FileContent::bytes(format!("a={a} rest={rest}\n")))
        }
    }
}

mod ambiguous_handlers {
    use super::*;

    pub struct AmbiguousHandlers;

    // Dir+file co-existence on identical rest-captured templates: the
    // child's kind (file vs directory) is content-determined, so the host
    // must defer to the parent dir's projection verdict at lookup time.
    #[omnifs_sdk::handlers]
    impl AmbiguousHandlers {
        #[omnifs_sdk::dir("/tree/{*path}")]
        async fn tree_dir(_path: String) -> Result<Projection> {
            let mut projection = Projection::new();
            projection.dir("d");
            projection.file("f.txt");
            projection.page(PageStatus::Exhaustive);
            Ok(projection)
        }

        #[omnifs_sdk::file("/tree/{*path}")]
        async fn tree_file(path: String) -> Result<FileContent> {
            Ok(FileContent::bytes(format!("file at {path}").into_bytes()))
        }
    }
}

#[omnifs_sdk::provider(mounts(
    crate::root_handlers::RootHandlers,
    crate::hello_handlers::HelloHandlers,
    crate::extras_handlers::ExtrasHandlers,
))]
impl TestProvider {
    fn init(_config: Config) -> (State, ProviderInfo) {
        (
            State,
            ProviderInfo {
                name: "test".into(),
                version: "0.1.0".into(),
                description: "test provider".into(),
            },
        )
    }

    fn capabilities() -> RequestedCapabilities {
        RequestedCapabilities {
            domains: Vec::new(),
            auth_types: Vec::new(),
            max_memory_mb: 16,
            needs_git: false,
            needs_websocket: false,
            needs_streaming: false,
            refresh_interval_secs: 0,
        }
    }
}

#[tokio::test]
async fn registry_uses_path_first_handlers() {
    use omnifs_sdk::browse::{List, Lookup};

    let mut registry = omnifs_sdk::__internal::MountRegistry::new();
    root_handlers::RootHandlers::mount(&mut registry);
    hello_handlers::HelloHandlers::mount(&mut registry);
    extras_handlers::ExtrasHandlers::mount(&mut registry);
    rest_handlers::RestHandlers::mount(&mut registry);
    registry.validate().unwrap();

    let cx = Cx::new(7, Rc::new(RefCell::new(State)));
    let list = registry.list_children(&cx, "/").await.unwrap();
    let List::Entries(listing) = list else {
        panic!("expected entries, got subtree");
    };
    assert!(
        listing
            .entries()
            .iter()
            .any(|entry| entry.name() == "hello")
    );

    let lookup = registry.lookup_child(&cx, "/hello", "world").await.unwrap();
    let Lookup::Entry(entry) = &lookup else {
        panic!("expected lookup entry, got {lookup:?}");
    };
    assert_eq!(entry.target().name(), "world");

    let file = registry.read_file(&cx, "/hello/world").await.unwrap();
    assert_eq!(file.content(), Some(&b"hello world\n"[..]));

    let projected = registry.read_file(&cx, "/bundle/title").await.unwrap();
    assert_eq!(projected.content(), Some(&b"bundle title\n"[..]));

    let checkout_list = registry.list_children(&cx, "/checkout").await.unwrap();
    assert!(matches!(checkout_list, List::Subtree(42)));

    let checkout_lookup = registry.lookup_child(&cx, "/", "checkout").await.unwrap();
    assert!(matches!(checkout_lookup, Lookup::Subtree(42)));

    // Rest-capture dispatch: multi-segment tails decode to the joined string.
    let rest_empty = registry.read_file(&cx, "/root/alpha").await.unwrap();
    assert_eq!(rest_empty.content(), Some(&b"a=alpha rest=\n"[..]));
    let rest_one = registry.read_file(&cx, "/root/alpha/beta").await.unwrap();
    assert_eq!(rest_one.content(), Some(&b"a=alpha rest=beta\n"[..]));
    let rest_deep = registry.read_file(&cx, "/root/alpha/b/c/d").await.unwrap();
    assert_eq!(rest_deep.content(), Some(&b"a=alpha rest=b/c/d\n"[..]));
}

fn parse_unit(path: &str) -> Option<Box<dyn std::any::Any>> {
    if path.is_empty() {
        None
    } else {
        Some(Box::new(()))
    }
}

fn call_dir<'a>(
    _cx: &'a Cx<State>,
    _path: Box<dyn std::any::Any>,
    _intent: DirIntent,
) -> omnifs_sdk::handler::BoxFuture<'a, Projection> {
    Box::pin(async { Ok(Projection::new()) })
}

#[test]
fn registry_rejects_ambiguous_dir_routes() {
    let mut registry = omnifs_sdk::__internal::MountRegistry::<State>::new();
    registry
        .add_dir("/items/{id}", parse_unit, call_dir)
        .unwrap();
    registry
        .add_dir("/items/{name}", parse_unit, call_dir)
        .unwrap();

    let error = registry.validate().unwrap_err();
    assert!(error.message().contains("ambiguous dir handlers"));
}

fn parse_path_only(path: &str) -> Option<Box<dyn std::any::Any>> {
    if path.is_empty() {
        None
    } else {
        Some(Box::new(path.to_string()))
    }
}

fn call_file_echo<'a>(
    _cx: &'a Cx<State>,
    path: Box<dyn std::any::Any>,
) -> omnifs_sdk::handler::BoxFuture<'a, FileContent> {
    Box::pin(async move {
        let path = *path.downcast::<String>().expect("file path mismatch");
        Ok(FileContent::bytes(path.into_bytes()))
    })
}

#[test]
fn registry_rejects_two_rest_patterns_at_same_prefix() {
    let mut registry = omnifs_sdk::__internal::MountRegistry::<State>::new();
    registry
        .add_file("/ipfs/{cid}/{*path}", parse_path_only, call_file_echo)
        .unwrap();
    registry
        .add_file("/ipfs/{cid}/{*tail}", parse_path_only, call_file_echo)
        .unwrap();

    let error = registry.validate().unwrap_err();
    assert!(error.message().contains("ambiguous file handlers"));
}

#[test]
fn registry_accepts_rest_alongside_exact_and_prefix() {
    let mut registry = omnifs_sdk::__internal::MountRegistry::<State>::new();
    registry
        .add_file("/ipfs/{cid}/versions", parse_path_only, call_file_echo)
        .unwrap();
    registry
        .add_file("/ipfs/{cid}/v{version}", parse_path_only, call_file_echo)
        .unwrap();
    registry
        .add_file("/ipfs/{cid}/{*path}", parse_path_only, call_file_echo)
        .unwrap();
    registry.validate().unwrap();
}

fn call_exact<'a>(
    _cx: &'a Cx<State>,
    _path: Box<dyn std::any::Any>,
) -> omnifs_sdk::handler::BoxFuture<'a, FileContent> {
    Box::pin(async { Ok(FileContent::bytes(b"exact".to_vec())) })
}

fn call_prefix<'a>(
    _cx: &'a Cx<State>,
    _path: Box<dyn std::any::Any>,
) -> omnifs_sdk::handler::BoxFuture<'a, FileContent> {
    Box::pin(async { Ok(FileContent::bytes(b"prefix".to_vec())) })
}

fn call_rest<'a>(
    _cx: &'a Cx<State>,
    _path: Box<dyn std::any::Any>,
) -> omnifs_sdk::handler::BoxFuture<'a, FileContent> {
    Box::pin(async { Ok(FileContent::bytes(b"rest".to_vec())) })
}

struct StubSubtree;

impl omnifs_sdk::handler::Handler<State> for StubSubtree {
    fn lookup_child<'a>(
        &'a self,
        _cx: &'a Cx<State>,
        _parent_path: &'a str,
        _name: &'a str,
    ) -> omnifs_sdk::handler::BoxFuture<'a, omnifs_sdk::browse::Lookup> {
        Box::pin(async { Ok(omnifs_sdk::browse::Lookup::not_found()) })
    }

    fn list_children<'a>(
        &'a self,
        _cx: &'a Cx<State>,
        _path: &'a str,
    ) -> omnifs_sdk::handler::BoxFuture<'a, omnifs_sdk::browse::List> {
        Box::pin(async {
            Ok(omnifs_sdk::browse::List::entries(
                omnifs_sdk::browse::Listing::empty_complete(),
            ))
        })
    }

    fn read_file<'a>(
        &'a self,
        _cx: &'a Cx<State>,
        _path: &'a str,
    ) -> omnifs_sdk::handler::BoxFuture<'a, omnifs_sdk::browse::FileContent> {
        Box::pin(async { Ok(omnifs_sdk::browse::FileContent::new(Vec::new())) })
    }
}

fn call_bind_stub<'a>(
    _cx: &'a Cx<State>,
    _parsed: Box<dyn std::any::Any>,
) -> omnifs_sdk::handler::BoxFuture<'a, Box<dyn omnifs_sdk::handler::Handler<State>>> {
    Box::pin(async { Ok(Box::new(StubSubtree) as Box<dyn omnifs_sdk::handler::Handler<State>>) })
}

// Regression: looking up a path that exactly matches a bind template must
// return a non-exhaustive Lookup. The host caches lookup-side projections
// keyed by the looked-up path; if the bind shortcut returns an exhaustive
// entry with no siblings, the host writes an exhaustive empty Dirents at
// that path and a subsequent readdir short-circuits before invoking the
// subtree's `list_children`.
#[tokio::test]
async fn bind_exact_match_lookup_is_not_exhaustive() {
    use omnifs_sdk::browse::Lookup;

    let mut registry = omnifs_sdk::__internal::MountRegistry::<State>::new();
    registry
        .add_bind("/papers/{paper}", parse_path_only, call_bind_stub)
        .unwrap();
    registry.validate().unwrap();

    let cx = Cx::new(11, Rc::new(RefCell::new(State)));
    let lookup = registry
        .lookup_child(&cx, "/papers", "1706.03762")
        .await
        .unwrap();
    let Lookup::Entry(entry) = &lookup else {
        panic!("expected lookup entry, got {lookup:?}");
    };
    assert!(
        !entry.is_exhaustive(),
        "bind exact-match must not claim an exhaustive sibling set"
    );
    assert!(entry.siblings().is_empty());
    assert!(entry.sibling_files().is_empty());
}

#[tokio::test]
async fn registry_prefers_exact_and_prefix_over_rest() {
    let mut registry = omnifs_sdk::__internal::MountRegistry::<State>::new();
    registry
        .add_file("/_ipfs/{cid}/versions", parse_path_only, call_exact)
        .unwrap();
    registry
        .add_file("/_ipfs/{cid}/v{version}", parse_path_only, call_prefix)
        .unwrap();
    registry
        .add_file("/_ipfs/{cid}/{*path}", parse_path_only, call_rest)
        .unwrap();
    registry.validate().unwrap();

    let cx = Cx::new(9, Rc::new(RefCell::new(State)));
    let exact = registry
        .read_file(&cx, "/_ipfs/Qm123/versions")
        .await
        .unwrap();
    assert_eq!(exact.content(), Some(&b"exact"[..]));
    let prefix = registry.read_file(&cx, "/_ipfs/Qm123/v1").await.unwrap();
    assert_eq!(prefix.content(), Some(&b"prefix"[..]));
    let rest = registry.read_file(&cx, "/_ipfs/Qm123/a/b/c").await.unwrap();
    assert_eq!(rest.content(), Some(&b"rest"[..]));
    let rest_empty = registry.read_file(&cx, "/_ipfs/Qm123").await.unwrap();
    assert_eq!(rest_empty.content(), Some(&b"rest"[..]));
}

#[tokio::test]
async fn implicit_prefix_dir_lookup_resolves_without_explicit_handler() {
    use omnifs_sdk::browse::{EntryKind, List, Lookup};

    let mut registry = omnifs_sdk::__internal::MountRegistry::<State>::new();
    registry
        .add_dir("/categories/{category}", parse_path_only, call_dir)
        .unwrap();
    registry
        .add_dir("/categories/{category}/{ym}", parse_path_only, call_dir)
        .unwrap();
    registry.validate().unwrap();

    let cx = Cx::new(13, Rc::new(RefCell::new(State)));

    // Implicit "/" has only literal children at depth 1 → exhaustive.
    let list = registry.list_children(&cx, "/").await.unwrap();
    let List::Entries(listing) = list else {
        panic!("expected entries, got subtree");
    };
    let names: Vec<&str> = listing.entries().iter().map(|e| e.name()).collect();
    assert_eq!(names, ["categories"]);
    assert!(listing.exhaustive());

    // Implicit "/categories" has only dynamic captures below → not exhaustive.
    let list = registry.list_children(&cx, "/categories").await.unwrap();
    let List::Entries(listing) = list else {
        panic!("expected entries, got subtree");
    };
    assert!(listing.entries().is_empty());
    assert!(
        !listing.exhaustive(),
        "implicit prefix dir with dynamic-capture children must not claim exhaustive"
    );

    let lookup = registry.lookup_child(&cx, "/", "categories").await.unwrap();
    let Lookup::Entry(entry) = &lookup else {
        panic!("expected lookup entry, got {lookup:?}");
    };
    assert_eq!(entry.target().name(), "categories");
    assert_eq!(entry.target().kind(), EntryKind::Directory);
    assert!(entry.is_exhaustive());

    let lookup = registry
        .lookup_child(&cx, "/categories", "cs.AI")
        .await
        .unwrap();
    let Lookup::Entry(entry) = &lookup else {
        panic!("expected lookup entry, got {lookup:?}");
    };
    assert_eq!(entry.target().name(), "cs.AI");
    assert_eq!(entry.target().kind(), EntryKind::Directory);
}

#[tokio::test]
async fn implicit_prefix_dir_with_only_capture_root_lookup_falls_through_to_dynamic() {
    use omnifs_sdk::browse::{EntryKind, List, Lookup};

    let mut registry = omnifs_sdk::__internal::MountRegistry::<State>::new();
    registry
        .add_dir("/{owner}", parse_path_only, call_dir)
        .unwrap();
    registry
        .add_dir("/{owner}/{repo}", parse_path_only, call_dir)
        .unwrap();
    registry.validate().unwrap();

    let cx = Cx::new(15, Rc::new(RefCell::new(State)));

    let list = registry.list_children(&cx, "/").await.unwrap();
    let List::Entries(listing) = list else {
        panic!("expected entries, got subtree");
    };
    assert!(listing.entries().is_empty());
    assert!(!listing.exhaustive());

    let lookup = registry.lookup_child(&cx, "/", "raulk").await.unwrap();
    let Lookup::Entry(entry) = &lookup else {
        panic!("expected lookup entry, got {lookup:?}");
    };
    assert_eq!(entry.target().name(), "raulk");
    assert_eq!(entry.target().kind(), EntryKind::Directory);
}

fn parse_only_digits(path: &str) -> Option<Box<dyn std::any::Any>> {
    let last = path.rsplit('/').next()?;
    if !last.is_empty() && last.chars().all(|c| c.is_ascii_digit()) {
        Some(Box::new(last.to_string()))
    } else {
        None
    }
}

fn call_digits<'a>(
    _cx: &'a Cx<State>,
    _path: Box<dyn std::any::Any>,
) -> omnifs_sdk::handler::BoxFuture<'a, FileContent> {
    Box::pin(async { Ok(FileContent::bytes(b"digits".to_vec())) })
}

#[tokio::test]
async fn parse_rejection_falls_through_to_next_candidate() {
    let mut registry = omnifs_sdk::__internal::MountRegistry::<State>::new();
    registry
        .add_file("/items/{id}", parse_only_digits, call_digits)
        .unwrap();
    registry
        .add_file("/items/{*tail}", parse_path_only, call_rest)
        .unwrap();
    registry.validate().unwrap();

    let cx = Cx::new(17, Rc::new(RefCell::new(State)));

    let digits = registry.read_file(&cx, "/items/42").await.unwrap();
    assert_eq!(digits.content(), Some(&b"digits"[..]));

    let alpha = registry.read_file(&cx, "/items/abc").await.unwrap();
    assert_eq!(alpha.content(), Some(&b"rest"[..]));
}
