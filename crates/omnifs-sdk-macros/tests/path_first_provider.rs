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

        #[omnifs_sdk::subtree("/checkout")]
        async fn checkout() -> Result<SubtreeRef> {
            Ok(SubtreeRef::new(42))
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
    assert_eq!(file.content(), b"hello world\n");

    let projected = registry.read_file(&cx, "/bundle/title").await.unwrap();
    assert_eq!(projected.content(), b"bundle title\n");

    let checkout_list = registry.list_children(&cx, "/checkout").await.unwrap();
    assert!(matches!(checkout_list, List::Subtree(42)));

    let checkout_lookup = registry.lookup_child(&cx, "/", "checkout").await.unwrap();
    assert!(matches!(checkout_lookup, Lookup::Subtree(42)));

    // Rest-capture dispatch: multi-segment tails decode to the joined string.
    let rest_empty = registry.read_file(&cx, "/root/alpha").await.unwrap();
    assert_eq!(rest_empty.content(), b"a=alpha rest=\n");
    let rest_one = registry.read_file(&cx, "/root/alpha/beta").await.unwrap();
    assert_eq!(rest_one.content(), b"a=alpha rest=beta\n");
    let rest_deep = registry.read_file(&cx, "/root/alpha/b/c/d").await.unwrap();
    assert_eq!(rest_deep.content(), b"a=alpha rest=b/c/d\n");
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

#[tokio::test]
async fn ambiguous_dir_file_routes_via_parent_projection() {
    use omnifs_sdk::browse::{EntryKind, List, Lookup};

    let mut registry = omnifs_sdk::__internal::MountRegistry::new();
    root_handlers::RootHandlers::mount(&mut registry);
    ambiguous_handlers::AmbiguousHandlers::mount(&mut registry);
    // The validator must accept dir+file on identical templates.
    registry.validate().unwrap();

    let cx = Cx::new(11, Rc::new(RefCell::new(State)));

    // lookup of "f.txt" under "/tree/foo" — the parent dir's projection
    // says "f.txt" is a file, so the lookup target is a file.
    let lookup = registry
        .lookup_child(&cx, "/tree/foo", "f.txt")
        .await
        .unwrap();
    let Lookup::Entry(entry) = &lookup else {
        panic!("expected lookup entry, got {lookup:?}");
    };
    assert_eq!(entry.target().name(), "f.txt");
    assert_eq!(entry.target().kind(), EntryKind::File);

    // lookup of "d" under "/tree/foo" — the parent's projection says
    // "d" is a directory.
    let lookup = registry.lookup_child(&cx, "/tree/foo", "d").await.unwrap();
    let Lookup::Entry(entry) = &lookup else {
        panic!("expected lookup entry, got {lookup:?}");
    };
    assert_eq!(entry.target().name(), "d");
    assert_eq!(entry.target().kind(), EntryKind::Directory);

    // read_file dispatches to the file handler with the rest-captured tail.
    let content = registry.read_file(&cx, "/tree/foo/f.txt").await.unwrap();
    assert_eq!(content.content(), b"file at foo/f.txt");

    // list_children dispatches to the dir handler.
    let listing = registry.list_children(&cx, "/tree/foo").await.unwrap();
    let List::Entries(listing) = listing else {
        panic!("expected entries, got subtree");
    };
    let names: Vec<&str> = listing.entries().iter().map(|e| e.name()).collect();
    assert!(names.contains(&"d"));
    assert!(names.contains(&"f.txt"));
}

#[test]
fn registry_rejects_subtree_alongside_dir_or_file_on_same_template() {
    let mut registry = omnifs_sdk::__internal::MountRegistry::<State>::new();
    registry.add_dir("/handoff", parse_unit, call_dir).unwrap();
    registry
        .add_subtree("/handoff", parse_unit, call_subtree)
        .unwrap();
    let error = registry.validate().unwrap_err();
    assert!(error.message().contains("conflicts with a subtree"));
}

fn call_subtree<'a>(
    _cx: &'a Cx<State>,
    _path: Box<dyn std::any::Any>,
) -> omnifs_sdk::handler::BoxFuture<'a, SubtreeRef> {
    Box::pin(async { Ok(SubtreeRef::new(1)) })
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
    assert_eq!(exact.content(), b"exact");
    let prefix = registry.read_file(&cx, "/_ipfs/Qm123/v1").await.unwrap();
    assert_eq!(prefix.content(), b"prefix");
    let rest = registry.read_file(&cx, "/_ipfs/Qm123/a/b/c").await.unwrap();
    assert_eq!(rest.content(), b"rest");
    let rest_empty = registry.read_file(&cx, "/_ipfs/Qm123").await.unwrap();
    assert_eq!(rest_empty.content(), b"rest");
}
