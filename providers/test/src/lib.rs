#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use omnifs_sdk::Cx;
use omnifs_sdk::prelude::*;
#[config]
struct Config {}

#[derive(Clone)]
struct State;

mod root_handlers {
    use super::*;

    pub struct RootHandlers;

    #[handlers]
    impl RootHandlers {
        #[dir("/")]
        fn root() -> Result<Projection> {
            let mut projection = Projection::new();
            projection.page(PageStatus::Exhaustive);
            Ok(projection)
        }
    }
}

mod hello_handlers {
    use super::*;

    fn hello_listing() -> Projection {
        let mut projection = Projection::new();
        projection.file("message");
        projection.file("greeting");
        projection.file("projected");
        projection.page(PageStatus::Exhaustive);
        projection
    }

    fn projected_file(name: &str) -> Result<Projection> {
        let mut projection = Projection::new();
        match name {
            "message" => projection.file_with_content("message", b"Hello, world!"),
            "greeting" => projection.file_with_content("greeting", b"Hi there!\n"),
            "projected" => {
                projection.file_with_content("projected", b"title\n");
                projection.file_with_content("body", b"body\n");
                projection.file_with_content("state", b"open\n");
            },
            _ => return Err(ProviderError::not_found("projected file not found")),
        }
        Ok(projection)
    }

    pub struct HelloHandlers;

    #[handlers]
    impl HelloHandlers {
        #[dir("/hello")]
        #[allow(clippy::needless_pass_by_value, clippy::unused_async)]
        async fn hello(cx: &DirCx<State>) -> Result<Projection> {
            match cx.intent() {
                DirIntent::ReadProjectedFile { name } => match name.as_str() {
                    "message" | "greeting" | "projected" => projected_file(name),
                    _ => Err(ProviderError::not_found("projected file not found")),
                },
                DirIntent::Lookup { .. } => Ok(hello_listing()),
                DirIntent::List { .. } => {
                    let mut projection = hello_listing();
                    projection.preload_dir("hello/bundle");
                    projection.preload_many([
                        ("hello/bundle/title", b"title".to_vec()),
                        ("hello/bundle/body", b"body".to_vec()),
                        ("hello/bundle/empty", Vec::new()),
                    ]);
                    Ok(projection)
                },
            }
        }

        #[file("/hello/lazy")]
        fn lazy() -> Result<FileContent> {
            Ok(FileContent::bytes("lazy\n"))
        }

        #[dir("/hello/bundle")]
        fn bundle() -> Result<Projection> {
            let mut projection = Projection::new();
            projection.file_with_content("title", b"title");
            projection.file_with_content("body", b"body");
            projection.page(PageStatus::Exhaustive);
            Ok(projection)
        }

        #[dir("/hello/snapshot")]
        fn snapshot() -> Result<Projection> {
            let mut projection = Projection::new();
            projection.file_with_content("status", b"open\n");
            projection.page(PageStatus::Exhaustive);
            Ok(projection)
        }

        #[dir("/hello/snapshot/comments")]
        fn snapshot_comments() -> Result<Projection> {
            let mut projection = Projection::new();
            projection.page(PageStatus::Exhaustive);
            Ok(projection)
        }
    }
}

mod scoped_handlers {
    use super::*;

    pub struct ScopedHandlers;

    #[handlers]
    impl ScopedHandlers {
        #[dir("/scoped")]
        fn scoped() -> Result<Projection> {
            let mut projection = Projection::new();
            projection.file_with_content("item", b"scoped\n");
            projection.page(PageStatus::Exhaustive);
            Ok(projection)
        }
    }
}

mod subtree_handlers {
    use super::*;

    pub struct SubtreeHandlers;

    #[handlers]
    impl SubtreeHandlers {
        #[treeref("/checkout")]
        fn checkout() -> Result<TreeRef> {
            Ok(TreeRef::new(777))
        }
    }
}

#[provider(mounts(
    crate::root_handlers::RootHandlers,
    crate::hello_handlers::HelloHandlers,
    crate::scoped_handlers::ScopedHandlers,
    crate::subtree_handlers::SubtreeHandlers,
))]
impl TestProvider {
    fn init(_config: Config) -> (State, ProviderInfo) {
        (
            State,
            ProviderInfo {
                name: "test-provider".into(),
                version: "0.1.0".into(),
                description: "A test provider with canned data".into(),
            },
        )
    }

    fn capabilities() -> RequestedCapabilities {
        RequestedCapabilities {
            domains: vec!["httpbin.org".into()],
            auth_types: vec![],
            max_memory_mb: 16,
            needs_git: false,
            needs_websocket: false,
            needs_streaming: false,
            refresh_interval_secs: 0,
        }
    }

    #[allow(clippy::unused_async)]
    async fn on_event(cx: Cx<State>, event: ProviderEvent) -> Result<EventOutcome> {
        let ProviderEvent::TimerTick(_) = event else {
            return Ok(EventOutcome::new());
        };
        let mut outcome = EventOutcome::new();
        for path in cx.active_paths(crate::scoped_handlers::ScopedPath::MOUNT_ID, |path| {
            Some(path.to_string())
        }) {
            outcome.invalidate_path(format!("{path}/item"));
        }
        Ok(outcome)
    }
}
