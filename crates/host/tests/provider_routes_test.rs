mod support;

use omnifs_host::cache::RecordKind;
use omnifs_host::omnifs::provider::log::Host as ProviderLogHost;
use omnifs_host::omnifs::provider::types::{
    Callout, CalloutResult, Effect, EntryKind, ErrorKind, Header, Host as ProviderHost,
    HttpRequest, HttpResponse, ListChildrenResult, LogEntry, LookupChildResult, OpResult,
    ProjBytes, ProviderEvent, ProviderStep, Stability,
};
use omnifs_host::runtime::RuntimeError;
use std::sync::OnceLock;
use support::{
    create_test_repo, make_engine, make_initialized_runtime, provider_wasm_path,
    try_make_runtime_from_config,
};
use wasmtime::component::{Component, HasData, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

fn project_paths(effects: &[Effect]) -> Vec<&str> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Project(entry) => Some(entry.path.as_str()),
            _ => None,
        })
        .collect()
}

fn project_inline_content<'a>(effects: &'a [Effect], path: &str) -> Option<&'a [u8]> {
    effects.iter().find_map(|effect| match effect {
        Effect::Project(entry) if entry.path == path => match &entry.kind {
            EntryKind::File(file) => match &file.bytes {
                ProjBytes::Inline(bytes) => Some(bytes.as_slice()),
                ProjBytes::Deferred(_) => None,
            },
            EntryKind::Directory => None,
        },
        _ => None,
    })
}

fn project_file_stability(effects: &[Effect], path: &str) -> Option<Stability> {
    effects.iter().find_map(|effect| match effect {
        Effect::Project(entry) if entry.path == path => match &entry.kind {
            EntryKind::File(file) => Some(file.attrs.stability),
            EntryKind::Directory => None,
        },
        _ => None,
    })
}

fn invalidate_prefixes(effects: &[Effect]) -> Vec<&str> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::InvalidatePrefix(prefix) => Some(prefix.as_str()),
            _ => None,
        })
        .collect()
}

fn seed_github_repo_cache(harness: &support::RuntimeHarness, owner: &str, repo: &str) {
    let cache_path = harness
        .clone_dir
        .path()
        .join("github.com")
        .join(owner)
        .join(repo);
    create_test_repo(&cache_path, "Hello from cache\n");
    std::fs::write(
        cache_path.join(".omnifs-clone-url"),
        format!("git@github.com:{owner}/{repo}.git"),
    )
    .unwrap();
}

struct TestHostState {
    wasi: WasiCtx,
    table: ResourceTable,
}

impl WasiView for TestHostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl ProviderHost for TestHostState {}

impl ProviderLogHost for TestHostState {
    fn log(&mut self, _entry: LogEntry) {}
}

impl HasData for TestHostState {
    type Data<'a> = &'a mut TestHostState;
}

struct GithubProviderSession {
    _engine: Engine,
    store: Store<TestHostState>,
    bindings: omnifs_host::Provider,
}

struct GithubProviderFixture {
    engine: Engine,
    component: Component,
}

fn github_provider_fixture() -> &'static GithubProviderFixture {
    static FIXTURE: OnceLock<GithubProviderFixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let engine = make_engine();
        let component =
            Component::from_file(&engine, provider_wasm_path("omnifs_provider_github.wasm"))
                .unwrap();
        GithubProviderFixture { engine, component }
    })
}

#[derive(Debug)]
struct StepOutcome {
    result: Option<OpResult>,
    effects: Vec<Effect>,
    callouts: Vec<Callout>,
}

impl StepOutcome {
    fn from_step(step: ProviderStep) -> Self {
        match step {
            ProviderStep::Returned(ret) => Self {
                result: Some(ret.result),
                effects: ret.effects,
                callouts: Vec::new(),
            },
            ProviderStep::Suspended(callouts) => Self {
                result: None,
                effects: Vec::new(),
                callouts,
            },
        }
    }

    fn is_suspended(&self) -> bool {
        self.result.is_none() && !self.callouts.is_empty()
    }
}

impl GithubProviderSession {
    fn new() -> Self {
        let fixture = github_provider_fixture();
        let mut linker = Linker::<TestHostState>::new(&fixture.engine);
        wasmtime_wasi::p2::add_to_linker_sync::<TestHostState>(&mut linker).unwrap();
        omnifs_host::Provider::add_to_linker::<TestHostState, TestHostState>(
            &mut linker,
            |state| state,
        )
        .unwrap();

        let mut store = Store::new(
            &fixture.engine,
            TestHostState {
                wasi: WasiCtxBuilder::new().build(),
                table: ResourceTable::new(),
            },
        );

        let bindings =
            omnifs_host::Provider::instantiate(&mut store, &fixture.component, &linker).unwrap();
        let init = bindings
            .omnifs_provider_lifecycle()
            .call_initialize(&mut store, b"{}")
            .unwrap();
        assert!(
            matches!(init.result, OpResult::Initialize(_)),
            "expected provider initialization, got {init:?}"
        );

        Self {
            _engine: fixture.engine.clone(),
            store,
            bindings,
        }
    }

    fn read_file(&mut self, id: u64, path: &str) -> StepOutcome {
        StepOutcome::from_step(
            self.bindings
                .omnifs_provider_browse()
                .call_read_file(&mut self.store, id, path)
                .unwrap(),
        )
    }

    fn list_children(&mut self, id: u64, path: &str) -> StepOutcome {
        StepOutcome::from_step(
            self.bindings
                .omnifs_provider_browse()
                .call_list_children(&mut self.store, id, path)
                .unwrap(),
        )
    }

    fn lookup_child(&mut self, id: u64, parent_path: &str, name: &str) -> StepOutcome {
        StepOutcome::from_step(
            self.bindings
                .omnifs_provider_browse()
                .call_lookup_child(&mut self.store, id, parent_path, name)
                .unwrap(),
        )
    }

    #[allow(clippy::needless_pass_by_value)]
    fn resume(&mut self, id: u64, outcomes: Vec<CalloutResult>) -> StepOutcome {
        StepOutcome::from_step(
            self.bindings
                .omnifs_provider_continuation()
                .call_resume(&mut self.store, id, &outcomes)
                .unwrap(),
        )
    }

    fn timer_tick_with_paths(
        &mut self,
        id: u64,
        active_paths: Vec<omnifs_host::omnifs::provider::types::ActivePathSet>,
    ) -> StepOutcome {
        StepOutcome::from_step(
            self.bindings
                .omnifs_provider_notify()
                .call_on_event(
                    &mut self.store,
                    id,
                    &ProviderEvent::TimerTick(
                        omnifs_host::omnifs::provider::types::TimerTickContext { active_paths },
                    ),
                )
                .unwrap(),
        )
    }
}

fn invoke_github_read_route(path: &str) -> StepOutcome {
    let mut session = GithubProviderSession::new();
    session.read_file(1, path)
}

#[allow(clippy::needless_pass_by_value)]
fn expect_fetch(response: StepOutcome) -> HttpRequest {
    let StepOutcome {
        result: None,
        callouts,
        ..
    } = &response
    else {
        panic!("expected callouts response, got {response:?}");
    };
    let [Callout::Fetch(request)] = callouts.as_slice() else {
        panic!("expected fetch callout, got {response:?}");
    };
    request.clone()
}

#[test]
fn dns_provider_rejects_invalid_default_resolver_config_during_initialize() {
    let error = match try_make_runtime_from_config(
        r#"
        {
            "provider": "omnifs_provider_dns.wasm",
            "mount": "dns",
            "capabilities": {
                "domains": ["cloudflare-dns.com", "dns.google"]
            },
            "config": {
                "default_resolver": "missing",
                "resolvers": {
                    "cloudflare": {
                        "url": "https://cloudflare-dns.com/dns-query",
                        "aliases": ["1.1.1.1"]
                    }
                }
            }
        }
    "#,
    ) {
        Ok(_) => panic!("expected runtime construction to fail for invalid dns config"),
        Err(error) => error,
    }
    .to_string();

    assert!(
        error.contains("default resolver"),
        "unexpected construction error: {error}"
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn dns_provider_routes_static_and_dynamic_paths() {
    let harness = make_initialized_runtime(
        r#"
        {
            "provider": "omnifs_provider_dns.wasm",
            "mount": "dns",
            "capabilities": {
                "domains": ["cloudflare-dns.com", "dns.google"]
            }
        }
    "#,
    );

    let lookup = harness
        .runtime
        .lookup_child("", "_resolvers")
        .await
        .unwrap();
    match lookup {
        LookupChildResult::Entry(result) => {
            let entry = &result.target;
            assert_eq!(entry.name, "_resolvers");
            assert!(matches!(entry.kind, EntryKind::File(_)));
        },
        other => panic!("expected Lookup, got {other:?}"),
    }

    let resolvers_file = harness.runtime.read_file("_resolvers").await.unwrap();
    let body =
        String::from_utf8(support::into_inline(resolvers_file)).expect("utf8 resolvers file");
    assert!(
        body.contains("cloudflare"),
        "unexpected resolvers file: {body}"
    );

    let reverse_lookup = harness.runtime.lookup_child("", "_reverse").await.unwrap();
    match reverse_lookup {
        LookupChildResult::Entry(result) => {
            let entry = &result.target;
            assert_eq!(entry.name, "_reverse");
            assert!(matches!(entry.kind, EntryKind::Directory));
        },
        other => panic!("expected Lookup, got {other:?}"),
    }

    let resolver_lookup = harness
        .runtime
        .lookup_child("", "@cloudflare")
        .await
        .unwrap();
    match resolver_lookup {
        LookupChildResult::Entry(result) => {
            let entry = &result.target;
            assert_eq!(entry.name, "@cloudflare");
            assert!(matches!(entry.kind, EntryKind::Directory));
        },
        other => panic!("expected Lookup, got {other:?}"),
    }

    let resolver_domain_lookup = harness
        .runtime
        .lookup_child("@cloudflare", "example.com")
        .await
        .unwrap();
    match resolver_domain_lookup {
        LookupChildResult::Entry(result) => {
            let entry = &result.target;
            assert_eq!(entry.name, "example.com");
            assert!(matches!(entry.kind, EntryKind::Directory));
        },
        other => panic!("expected Lookup, got {other:?}"),
    }

    let resolver_reverse_lookup = harness
        .runtime
        .lookup_child("@cloudflare", "_reverse")
        .await
        .unwrap();
    match resolver_reverse_lookup {
        LookupChildResult::Entry(result) => {
            let entry = &result.target;
            assert_eq!(entry.name, "_reverse");
            assert!(matches!(entry.kind, EntryKind::Directory));
        },
        other => panic!("expected resolver reverse lookup, got {other:?}"),
    }

    let reverse_ip_lookup = harness
        .runtime
        .lookup_child("_reverse", "8.8.8.8")
        .await
        .unwrap();
    match reverse_ip_lookup {
        LookupChildResult::Entry(result) => {
            let entry = &result.target;
            assert_eq!(entry.name, "8.8.8.8");
            assert!(matches!(entry.kind, EntryKind::File(_)));
            assert!(result.siblings.is_empty());
        },
        other => panic!("expected reverse IP lookup, got {other:?}"),
    }

    let resolver_reverse_ip_lookup = harness
        .runtime
        .lookup_child("@cloudflare/_reverse", "8.8.8.8")
        .await
        .unwrap();
    match resolver_reverse_ip_lookup {
        LookupChildResult::Entry(result) => {
            let entry = &result.target;
            assert_eq!(entry.name, "8.8.8.8");
            assert!(matches!(entry.kind, EntryKind::File(_)));
            assert!(result.siblings.is_empty());
        },
        other => panic!("expected resolver-qualified reverse IP lookup, got {other:?}"),
    }

    let invalid_reverse_lookup = harness
        .runtime
        .lookup_child("_reverse", "not-an-ip")
        .await
        .unwrap();
    match invalid_reverse_lookup {
        LookupChildResult::NotFound => {},
        other => panic!("expected invalid reverse lookup NotFound, got {other:?}"),
    }

    let invalid_resolver_reverse_lookup = harness
        .runtime
        .lookup_child("@cloudflare/_reverse", "not-an-ip")
        .await
        .unwrap();
    match invalid_resolver_reverse_lookup {
        LookupChildResult::NotFound => {},
        other => panic!("expected invalid resolver reverse lookup NotFound, got {other:?}"),
    }

    let direct_ip_lookup = harness.runtime.lookup_child("", "8.8.8.8").await.unwrap();
    match direct_ip_lookup {
        LookupChildResult::NotFound => {},
        other => panic!("expected root direct-IP lookup NotFound, got {other:?}"),
    }

    let resolver_direct_ip_lookup = harness
        .runtime
        .lookup_child("@cloudflare", "8.8.8.8")
        .await
        .unwrap();
    match resolver_direct_ip_lookup {
        LookupChildResult::NotFound => {},
        other => panic!("expected resolver direct-IP lookup NotFound, got {other:?}"),
    }

    let domain_lookup = harness
        .runtime
        .lookup_child("", "example.com")
        .await
        .unwrap();
    match domain_lookup {
        LookupChildResult::Entry(result) => {
            let entry = &result.target;
            assert_eq!(entry.name, "example.com");
            assert!(matches!(entry.kind, EntryKind::Directory));
            assert!(
                harness
                    .runtime
                    .cache_get("example.com/A", RecordKind::Lookup, None)
                    .is_some()
            );
            assert!(
                harness
                    .runtime
                    .cache_get("example.com/AAAA", RecordKind::Lookup, None)
                    .is_some()
            );
            assert!(
                harness
                    .runtime
                    .cache_get("example.com/_all", RecordKind::Lookup, None)
                    .is_some()
            );
            assert!(
                harness
                    .runtime
                    .cache_get("example.com/_raw", RecordKind::Lookup, None)
                    .is_some()
            );
        },
        other => panic!("expected domain lookup, got {other:?}"),
    }

    let listing = harness.runtime.list_children("example.com").await.unwrap();
    match listing {
        ListChildrenResult::Entries(listing) => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert!(names.contains(&"A"));
            assert!(names.contains(&"_all"));
            assert!(names.contains(&"_raw"));
        },
        other @ ListChildrenResult::Subtree(_) => {
            panic!("expected domain listing, got {other:?}")
        },
    }

    let reverse_listing = harness.runtime.list_children("_reverse").await.unwrap();
    match reverse_listing {
        ListChildrenResult::Entries(listing) => {
            assert!(
                listing.entries.is_empty(),
                "reverse dir should not eagerly list dynamic children: {listing:?}"
            );
        },
        other @ ListChildrenResult::Subtree(_) => {
            panic!("expected reverse dir listing, got {other:?}")
        },
    }

    let resolver_reverse_listing = harness
        .runtime
        .list_children("@cloudflare/_reverse")
        .await
        .unwrap();
    match resolver_reverse_listing {
        ListChildrenResult::Entries(listing) => {
            assert!(
                listing.entries.is_empty(),
                "resolver reverse dir should not eagerly list dynamic children: {listing:?}"
            );
        },
        other @ ListChildrenResult::Subtree(_) => {
            panic!("expected resolver reverse dir listing, got {other:?}")
        },
    }
}

#[tokio::test]
async fn dns_provider_activity_tracks_concrete_dispatched_paths() {
    let harness = make_initialized_runtime(
        r#"
        {
            "provider": "omnifs_provider_dns.wasm",
            "mount": "dns",
            "capabilities": {
                "domains": ["cloudflare-dns.com", "dns.google"]
            }
        }
    "#,
    );

    harness.runtime.read_file("_resolvers").await.unwrap();

    harness
        .runtime
        .lookup_child("@cloudflare", "example.com")
        .await
        .unwrap();

    harness
        .runtime
        .lookup_child("_reverse", "8.8.8.8")
        .await
        .unwrap();

    harness
        .runtime
        .lookup_child("@cloudflare/_reverse", "8.8.8.8")
        .await
        .unwrap();

    let active = harness.runtime.__active_path_sets();

    let root = active
        .iter()
        .find(|entry| entry.mount_id == "/")
        .expect("missing root activity");
    assert_eq!(root.paths, vec!["/"]);

    let resolvers = active
        .iter()
        .find(|entry| entry.mount_id == "/_resolvers")
        .expect("missing resolvers activity");
    assert_eq!(resolvers.paths, vec!["/_resolvers"]);

    let resolver_root = active
        .iter()
        .find(|entry| entry.mount_id == "/@{resolver}")
        .unwrap_or_else(|| panic!("missing resolver-root activity in {active:?}"));
    assert_eq!(resolver_root.paths, vec!["/@cloudflare"]);

    let dns_segment = active
        .iter()
        .find(|entry| entry.mount_id == "/@{resolver}/{domain}")
        .expect("missing dns-segment activity");
    assert_eq!(dns_segment.paths, vec!["/@cloudflare/example.com"]);
    assert!(!dns_segment.paths.iter().any(|path| path == "/_resolvers"));
    assert!(!dns_segment.paths.iter().any(|path| path == "/@cloudflare"));

    let reverse_dir = active
        .iter()
        .find(|entry| entry.mount_id == "/_reverse")
        .expect("missing reverse-dir activity");
    assert_eq!(reverse_dir.paths, vec!["/_reverse"]);

    let resolver_reverse_dir = active
        .iter()
        .find(|entry| entry.mount_id == "/@{resolver}/_reverse")
        .expect("missing resolver-reverse-dir activity");
    assert_eq!(resolver_reverse_dir.paths, vec!["/@cloudflare/_reverse"]);

    let reverse_ip = active
        .iter()
        .find(|entry| entry.mount_id == "/_reverse/{ip}")
        .expect("missing reverse-ip activity");
    assert_eq!(reverse_ip.paths, vec!["/_reverse/8.8.8.8"]);

    let resolver_reverse_ip = active
        .iter()
        .find(|entry| entry.mount_id == "/@{resolver}/_reverse/{ip}")
        .expect("missing resolver-reverse-ip activity");
    assert_eq!(
        resolver_reverse_ip.paths,
        vec!["/@cloudflare/_reverse/8.8.8.8"]
    );

    assert!(
        !dns_segment
            .paths
            .iter()
            .any(|path| path.contains("/_reverse")),
        "dns segment activity should stay domain-only: {active:?}"
    );
}

#[tokio::test]
async fn dns_provider_unknown_resolver_read_is_invalid_input() {
    let harness = make_initialized_runtime(
        r#"
        {
            "provider": "omnifs_provider_dns.wasm",
            "mount": "dns",
            "capabilities": {
                "domains": ["cloudflare-dns.com", "dns.google"]
            }
        }
    "#,
    );

    let error = harness
        .runtime
        .read_file("@missing/example.com/A")
        .await
        .unwrap_err();
    match error {
        RuntimeError::ProviderError(error) => {
            assert_eq!(error.kind, ErrorKind::InvalidInput);
            assert!(
                error.message.contains("unknown resolver specifier"),
                "unexpected resolver error: {error:?}"
            );
        },
        other => panic!("expected invalid-input resolver error, got {other:?}"),
    }
}

#[tokio::test]
async fn dns_provider_unknown_record_reads_are_not_found() {
    let harness = make_initialized_runtime(
        r#"
        {
            "provider": "omnifs_provider_dns.wasm",
            "mount": "dns",
            "capabilities": {
                "domains": ["cloudflare-dns.com", "dns.google"]
            }
        }
    "#,
    );

    let error = harness
        .runtime
        .read_file("example.com/BOGUS")
        .await
        .unwrap_err();
    match error {
        RuntimeError::ProviderError(error) => {
            assert_eq!(error.kind, ErrorKind::NotFound);
        },
        other => panic!("expected unknown-record NotFound, got {other:?}"),
    }

    let error = harness
        .runtime
        .read_file("@cloudflare/example.com/BOGUS")
        .await
        .unwrap_err();
    match error {
        RuntimeError::ProviderError(error) => {
            assert_eq!(error.kind, ErrorKind::NotFound);
        },
        other => panic!("expected resolver unknown-record NotFound, got {other:?}"),
    }
}

#[test]
fn github_provider_routes_namespace_and_numeric_paths() {
    let mut session = GithubProviderSession::new();

    let repo_listing = session.list_children(5, "octocat/Hello-World");
    match repo_listing {
        StepOutcome {
            result: Some(OpResult::ListChildren(ListChildrenResult::Entries(listing))),
            ..
        } => {
            let mut names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            names.sort_unstable();
            assert_eq!(names, vec!["_actions", "_issues", "_prs", "_repo"]);
        },
        other => panic!("expected repo namespace listing, got {other:?}"),
    }

    let runs_fetch = expect_fetch(session.lookup_child(6, "octocat/Hello-World/_actions", "runs"));
    assert!(
        runs_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/actions/runs?per_page=30"),
        "unexpected action runs listing URL: {}",
        runs_fetch.url
    );

    let lookup = session.resume(
        6,
        vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::<Header>::new(),
            body: br#"{
                "workflow_runs": [
                    {"id":123,"status":"completed","conclusion":"success"}
                ]
            }"#
            .to_vec(),
        })],
    );
    match lookup {
        StepOutcome {
            result: Some(OpResult::LookupChild(LookupChildResult::Entry(result))),
            ..
        } => {
            let entry = &result.target;
            assert_eq!(entry.name, "runs");
            assert!(matches!(entry.kind, EntryKind::Directory));
        },
        other => panic!("expected Lookup(runs), got {other:?}"),
    }

    // Note: projection-derived file lookups (`.../1/title`, `.../1/diff`)
    // are intentionally not asserted here. These files do not have
    // dedicated provider lookup handlers; the host's FuseFs resolves
    // them positively from
    // the parent's cached sibling entries (see d4e9e98's
    // dirents-implied positive path). `ProviderRuntime::call_lookup_child`
    // bypasses that cache and dispatches straight to the provider, so
    // it would return NotFound for them in isolation. Read-path
    // coverage for the same leaves lives in
    // `github_provider_read_routes_dispatch_async_handlers` and
    // `github_provider_resource_reads_do_not_fall_back_to_provider_cache`.
}

#[test]
fn github_issue_list_projects_files() {
    use omnifs_host::omnifs::provider::types::{Callout, CalloutResult, HttpResponse};

    let mut session = GithubProviderSession::new();
    let response = session.list_children(40, "octocat/Hello-World/_issues/_open");
    assert!(
        response.is_suspended(),
        "expected suspended response, got {response:?}"
    );
    let [Callout::Fetch(fetch)] = response.callouts.as_slice() else {
        panic!("expected fetch callout, got {:?}", response.callouts);
    };
    assert!(
        fetch.url.ends_with(
            "/search/issues?q=repo:octocat/Hello-World+state:open&sort=created&order=desc&per_page=100"
        ),
        "unexpected issues list URL: {}",
        fetch.url
    );

    let response = session.resume(
        40,
        vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{
                "total_count": 2,
                "items": [
                    {
                        "number":6,
                        "title":"PR title",
                        "body":null,
                        "state":"open",
                        "user":null,
                        "pull_request":{"url":"https://api.github.test/pulls/6"}
                    },
                    {
                        "number":7,
                        "title":"Issue title",
                        "body":"Issue body",
                        "state":"open",
                        "user":null
                    }
                ]
            }"#
            .to_vec(),
        })],
    );

    // Projection effects ride alongside the terminal listing, not as callouts.
    assert!(
        response.callouts.is_empty(),
        "list terminal should carry no callouts, got {:?}",
        response.callouts
    );
    match &response.result {
        Some(OpResult::ListChildren(ListChildrenResult::Entries(listing))) => {
            let project_paths = project_paths(&response.effects);
            assert_eq!(
                project_paths,
                vec![
                    "octocat/Hello-World/_prs/_open/6",
                    "octocat/Hello-World/_prs/_open/6/title",
                    "octocat/Hello-World/_prs/_open/6/body",
                    "octocat/Hello-World/_prs/_open/6/state",
                    "octocat/Hello-World/_prs/_open/6/user",
                    "octocat/Hello-World/_prs/_open/6/comments",
                    "octocat/Hello-World/_prs/_open/6/diff",
                    "octocat/Hello-World/_issues/_open/7/title",
                    "octocat/Hello-World/_issues/_open/7/body",
                    "octocat/Hello-World/_issues/_open/7/state",
                    "octocat/Hello-World/_issues/_open/7/user",
                ]
            );
            assert_eq!(
                project_inline_content(&response.effects, "octocat/Hello-World/_prs/_open/6/body"),
                None
            );
            assert_eq!(
                project_inline_content(
                    &response.effects,
                    "octocat/Hello-World/_issues/_open/7/body"
                ),
                None
            );
            assert_eq!(
                project_inline_content(&response.effects, "octocat/Hello-World/_prs/_open/6/user"),
                Some(&[][..])
            );
            assert_eq!(
                project_inline_content(
                    &response.effects,
                    "octocat/Hello-World/_issues/_open/7/user"
                ),
                Some(&[][..])
            );
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert_eq!(names, vec!["7"]);
        },
        other => panic!("expected issue listing terminal, got {other:?}"),
    }
}

#[test]
fn github_issue_list_scans_past_pr_only_pages() {
    use omnifs_host::omnifs::provider::types::{Callout, CalloutResult, HttpResponse};

    let mut session = GithubProviderSession::new();
    let response = session.list_children(42, "octocat/Hello-World/_issues/_all");
    let [Callout::Fetch(fetch)] = response.callouts.as_slice() else {
        panic!("expected first issues fetch, got {:?}", response.callouts);
    };
    assert!(fetch.url.ends_with(
        "/search/issues?q=repo:octocat/Hello-World&sort=created&order=desc&per_page=100"
    ));

    let response = session.resume(
        42,
        vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{
                "total_count": 150,
                "items": [
                    {
                        "number":6,
                        "title":"Recent PR",
                        "body":"PR body",
                        "state":"open",
                        "user":{"login":"hubot"},
                        "pull_request":{"url":"https://api.github.test/pulls/6"}
                    }
                ]
            }"#
            .to_vec(),
        })],
    );
    let [Callout::Fetch(fetch)] = response.callouts.as_slice() else {
        panic!(
            "expected second issues page fetch, got {:?}",
            response.callouts
        );
    };
    assert!(
        fetch.url.ends_with(
            "/repos/octocat/Hello-World/issues?state=all&sort=created&direction=desc&per_page=100&page=2"
        ),
        "unexpected REST page URL: {}",
        fetch.url
    );

    let response = session.resume(
        42,
        vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"[
                {
                    "number":7,
                    "title":"Older issue",
                    "body":"Issue body",
                    "state":"open",
                    "user":{"login":"octocat"}
                }
            ]"#
            .to_vec(),
        })],
    );
    match &response.result {
        Some(OpResult::ListChildren(ListChildrenResult::Entries(listing))) => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert_eq!(names, vec!["7"]);
            let project_paths = project_paths(&response.effects);
            assert!(project_paths.contains(&"octocat/Hello-World/_prs/_all/6"));
            assert!(project_paths.contains(&"octocat/Hello-World/_issues/_all/7/title"));
            assert!(listing.exhaustive);
        },
        other => panic!("expected issue listing terminal, got {other:?}"),
    }
}

#[test]
fn github_issue_list_dedupes_overlap_at_search_rest_seam() {
    use omnifs_host::omnifs::provider::types::{Callout, CalloutResult, HttpResponse};

    let mut session = GithubProviderSession::new();
    let response = session.list_children(43, "octocat/Hello-World/_issues/_all");
    let [Callout::Fetch(_)] = response.callouts.as_slice() else {
        panic!("expected first issues fetch, got {:?}", response.callouts);
    };

    let response = session.resume(
        43,
        vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{
                "total_count": 150,
                "items": [
                    {"number":11,"title":"Search-only","body":"a","state":"open","user":{"login":"o"}},
                    {"number":10,"title":"Boundary","body":"b","state":"open","user":{"login":"o"}}
                ]
            }"#
            .to_vec(),
        })],
    );
    let [Callout::Fetch(_)] = response.callouts.as_slice() else {
        panic!("expected REST page-2 fetch, got {:?}", response.callouts);
    };

    let response = session.resume(
        43,
        vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"[
                {"number":10,"title":"Boundary","body":"b","state":"open","user":{"login":"o"}},
                {"number":9,"title":"REST-only","body":"c","state":"open","user":{"login":"o"}}
            ]"#
            .to_vec(),
        })],
    );
    match &response.result {
        Some(OpResult::ListChildren(ListChildrenResult::Entries(listing))) => {
            let mut names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            names.sort_unstable();
            assert_eq!(names, vec!["10", "11", "9"]);
        },
        other => panic!("expected deduped issue listing, got {other:?}"),
    }
}

#[test]
fn github_pr_list_projects_files() {
    use omnifs_host::omnifs::provider::types::{Callout, CalloutResult, HttpResponse};

    let mut session = GithubProviderSession::new();
    let response = session.list_children(41, "octocat/Hello-World/_prs/_open");
    assert!(
        response.is_suspended(),
        "expected suspended response, got {response:?}"
    );
    let [Callout::Fetch(fetch)] = response.callouts.as_slice() else {
        panic!("expected fetch callout, got {:?}", response.callouts);
    };
    assert!(
        fetch.url.ends_with(
            "/search/issues?q=repo:octocat/Hello-World+is:pr+state:open&sort=created&order=desc&per_page=100"
        ),
        "unexpected PR list URL: {}",
        fetch.url
    );

    let response = session.resume(
        41,
        vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{
                "total_count": 1,
                "items": [
                    {
                        "number":7,
                        "title":"PR title",
                        "body":"PR body",
                        "state":"open",
                        "user":{"login":"octocat"}
                    }
                ]
            }"#
            .to_vec(),
        })],
    );

    assert!(
        response.callouts.is_empty(),
        "list terminal should carry no callouts, got {:?}",
        response.callouts
    );
    match &response.result {
        Some(OpResult::ListChildren(ListChildrenResult::Entries(listing))) => {
            let project_paths = project_paths(&response.effects);
            assert_eq!(
                project_paths,
                vec![
                    "octocat/Hello-World/_prs/_open/7/title",
                    "octocat/Hello-World/_prs/_open/7/body",
                    "octocat/Hello-World/_prs/_open/7/state",
                    "octocat/Hello-World/_prs/_open/7/user",
                ]
            );
            assert_eq!(
                project_inline_content(&response.effects, "octocat/Hello-World/_prs/_open/7/body"),
                None
            );
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert_eq!(names, vec!["7"]);
        },
        other => panic!("expected PR listing terminal, got {other:?}"),
    }
}

#[test]
fn github_action_run_list_projects_files() {
    use omnifs_host::omnifs::provider::types::{Callout, CalloutResult, HttpResponse};

    let mut session = GithubProviderSession::new();
    let response = session.list_children(42, "octocat/Hello-World/_actions/runs");
    assert!(
        response.is_suspended(),
        "expected suspended response, got {response:?}"
    );
    let [Callout::Fetch(fetch)] = response.callouts.as_slice() else {
        panic!("expected fetch callout, got {:?}", response.callouts);
    };
    assert!(
        fetch
            .url
            .ends_with("/repos/octocat/Hello-World/actions/runs?per_page=30"),
        "unexpected action runs URL: {}",
        fetch.url
    );

    let response = session.resume(
        42,
        vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{
                "workflow_runs":[
                    {
                        "id":123,
                        "status":"completed",
                        "conclusion":"success"
                    }
                ]
            }"#
            .to_vec(),
        })],
    );

    assert!(
        response.callouts.is_empty(),
        "list terminal should carry no callouts, got {:?}",
        response.callouts
    );
    match &response.result {
        Some(OpResult::ListChildren(ListChildrenResult::Entries(listing))) => {
            let project_paths = project_paths(&response.effects);
            assert_eq!(
                project_paths,
                vec![
                    "octocat/Hello-World/_actions/runs/123/status",
                    "octocat/Hello-World/_actions/runs/123/conclusion",
                ]
            );
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert_eq!(names, vec!["123"]);
        },
        other => panic!("expected action run listing terminal, got {other:?}"),
    }
}

#[test]
fn github_provider_action_run_lookup_validates_and_listing_validates() {
    use omnifs_host::omnifs::provider::types::{
        Callout, CalloutResult, Header, HttpRequest, HttpResponse,
    };

    #[allow(clippy::needless_pass_by_value)]
    fn expect_fetch(response: StepOutcome) -> HttpRequest {
        let StepOutcome {
            result: None,
            callouts,
            ..
        } = &response
        else {
            panic!("expected callouts response, got {response:?}");
        };
        let [Callout::Fetch(request)] = callouts.as_slice() else {
            panic!("expected fetch callout, got {response:?}");
        };
        request.clone()
    }

    let mut session = GithubProviderSession::new();

    let lookup_fetch =
        expect_fetch(session.lookup_child(7, "octocat/Hello-World/_actions/runs", "123"));
    assert!(
        lookup_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/actions/runs/123"),
        "unexpected action run lookup URL: {}",
        lookup_fetch.url
    );

    let lookup = session.resume(
        7,
        vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::<Header>::new(),
            body: br#"{"id":123,"status":"completed","conclusion":"success"}"#.to_vec(),
        })],
    );
    match lookup {
        StepOutcome {
            result: Some(OpResult::LookupChild(LookupChildResult::Entry(result))),
            effects,
            ..
        } => {
            let entry = &result.target;
            assert_eq!(entry.name, "123");
            assert!(matches!(entry.kind, EntryKind::Directory));
            let child_names = project_paths(&effects);
            assert!(
                child_names.contains(&"octocat/Hello-World/_actions/runs/123/status"),
                "missing status in {child_names:?}"
            );
            assert!(
                child_names.contains(&"octocat/Hello-World/_actions/runs/123/conclusion"),
                "missing conclusion in {child_names:?}"
            );
            assert!(
                child_names.contains(&"octocat/Hello-World/_actions/runs/123/log"),
                "missing log in {child_names:?}"
            );
        },
        other => panic!("expected validated action run lookup result, got {other:?}"),
    }

    let issued = session.list_children(7, "octocat/Hello-World/_actions/runs/123");
    assert!(
        issued.is_suspended(),
        "expected action run listing to dispatch validation, got {issued:?}"
    );

    let listed = session.resume(
        7,
        vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::<Header>::new(),
            body: br#"{"id":123,"status":"completed","conclusion":"success"}"#.to_vec(),
        })],
    );

    match listed {
        StepOutcome {
            result: Some(OpResult::ListChildren(ListChildrenResult::Entries(result))),
            ..
        } => {
            let names: Vec<&str> = result
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert!(names.contains(&"log"), "missing log in {names:?}");
        },
        other => panic!("expected list entries(123) after 200, got {other:?}"),
    }
}

#[test]
fn github_owner_listing_tracks_browsed_repos() {
    let mut session = GithubProviderSession::new();

    let repo_listing = session.list_children(44, "octocat/Hello-World");
    assert!(
        matches!(
            repo_listing,
            StepOutcome {
                result: Some(OpResult::ListChildren(_)),
                ..
            }
        ),
        "expected repo listing, got {repo_listing:?}"
    );

    let user_fetch = expect_fetch(session.list_children(45, "octocat"));
    assert!(
        user_fetch.url.ends_with("/users/octocat"),
        "expected owner user lookup first, got {}",
        user_fetch.url
    );
    let repos_fetch = expect_fetch(session.resume(
        45,
        vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::<Header>::new(),
            body: br#"{"login":"octocat","type":"User"}"#.to_vec(),
        })],
    ));
    assert!(
        repos_fetch
            .url
            .ends_with("/users/octocat/repos?per_page=100&sort=updated&page=1"),
        "expected owner repo listing fetch, got {}",
        repos_fetch.url
    );

    let owner_listing = session.resume(
        45,
        vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::<Header>::new(),
            body: br#"[{"name":"Hello-World"}]"#.to_vec(),
        })],
    );
    match owner_listing {
        StepOutcome {
            result: Some(OpResult::ListChildren(ListChildrenResult::Entries(listing))),
            ..
        } => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert!(
                names.contains(&"Hello-World"),
                "expected Hello-World in owner listing, got {names:?}"
            );
        },
        other => panic!("expected owner listing, got {other:?}"),
    }
}

#[test]
fn github_root_and_owner_listings_ignore_unclassified_repo_paths() {
    let mut session = GithubProviderSession::new();

    for (id, path) in [
        (46, "zeta/zulu"),
        (47, "open/source"),
        (48, "alpha/app"),
        (49, "openai/api"),
    ] {
        let repo_listing = session.list_children(id, path);
        assert!(
            matches!(
                repo_listing,
                StepOutcome {
                    result: Some(OpResult::ListChildren(_)),
                    ..
                }
            ),
            "expected repo listing for {path}, got {repo_listing:?}"
        );
    }

    let root_listing = session.list_children(50, "");
    match root_listing {
        StepOutcome {
            result: Some(OpResult::ListChildren(ListChildrenResult::Entries(listing))),
            ..
        } => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert!(names.is_empty(), "unexpected root names: {names:?}");
        },
        other => panic!("expected root listing, got {other:?}"),
    }

    let user_fetch = expect_fetch(session.list_children(51, "open"));
    assert!(
        user_fetch.url.ends_with("/users/open"),
        "expected owner user lookup first, got {}",
        user_fetch.url
    );
    let repos_fetch = expect_fetch(session.resume(
        51,
        vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::<Header>::new(),
            body: br#"{"login":"open","type":"User"}"#.to_vec(),
        })],
    ));
    assert!(
        repos_fetch
            .url
            .ends_with("/users/open/repos?per_page=100&sort=updated&page=1"),
        "expected owner repo listing fetch, got {}",
        repos_fetch.url
    );

    let owner_listing = session.resume(
        51,
        vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::<Header>::new(),
            body: b"[]".to_vec(),
        })],
    );
    match owner_listing {
        StepOutcome {
            result: Some(OpResult::ListChildren(ListChildrenResult::Entries(listing))),
            ..
        } => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert!(
                names.is_empty(),
                "unexpected owner names after uncached repo traversal: {names:?}"
            );
        },
        other => panic!("expected owner listing, got {other:?}"),
    }
}

#[tokio::test]
async fn github_repo_tree_lists_looks_up_and_reads_from_git_cache() {
    let harness = make_initialized_runtime(
        r#"
        {
            "provider": "omnifs_provider_github.wasm",
            "mount": "github",
            "capabilities": {
                "domains": ["api.github.com"],
                "git_repos": ["git@github.com:octocat/Hello-World.git"]
            }
        }
    "#,
    );
    seed_github_repo_cache(&harness, "octocat", "Hello-World");

    let repo_listing = harness
        .runtime
        .list_children("octocat/Hello-World/_repo")
        .await
        .unwrap();
    match repo_listing {
        ListChildrenResult::Subtree(tree_ref) => {
            let real_root = harness
                .runtime
                .resolve_tree_ref(tree_ref)
                .expect("missing disowned repo tree");
            assert!(real_root.join("README.md").is_file());
            assert!(real_root.join("src").is_dir());
        },
        other @ ListChildrenResult::Entries(_) => {
            panic!("expected repo tree listing, got {other:?}")
        },
    }

    let repo_child = harness
        .runtime
        .lookup_child("octocat/Hello-World", "_repo")
        .await
        .unwrap();
    match repo_child {
        LookupChildResult::Subtree(tree_ref) => {
            let real_root = harness
                .runtime
                .resolve_tree_ref(tree_ref)
                .expect("missing disowned repo tree");
            assert!(real_root.join("README.md").is_file());
            assert!(real_root.join("src").is_dir());
            assert_eq!(
                std::fs::read(real_root.join("README.md")).unwrap(),
                b"Hello from cache\n"
            );
            assert!(real_root.join("src/main.rs").is_file());
        },
        other => panic!("expected repo child lookup, got {other:?}"),
    }
}

#[test]
fn github_provider_missing_numbered_resources_validate_on_lookup() {
    use omnifs_host::omnifs::provider::types::{
        Callout, CalloutResult, ErrorKind, Header, HttpRequest, HttpResponse,
    };

    #[allow(clippy::needless_pass_by_value)]
    fn expect_fetch(response: StepOutcome) -> HttpRequest {
        let StepOutcome {
            result: None,
            callouts,
            ..
        } = &response
        else {
            panic!("expected callouts response, got {response:?}");
        };
        let [Callout::Fetch(request)] = callouts.as_slice() else {
            panic!("expected fetch callout, got {response:?}");
        };
        request.clone()
    }

    let mut session = GithubProviderSession::new();

    let issued =
        expect_fetch(session.lookup_child(1, "octocat/Hello-World/_issues/_open", "999999999"));
    assert!(
        issued
            .url
            .ends_with("/repos/octocat/Hello-World/issues/999999999"),
        "unexpected issue lookup URL: {}",
        issued.url
    );

    let response = session.resume(
        1,
        vec![CalloutResult::HttpResponse(HttpResponse {
            status: 404,
            headers: Vec::<Header>::new(),
            body: b"{\"message\":\"Not Found\"}".to_vec(),
        })],
    );

    match response {
        StepOutcome {
            result: Some(OpResult::Error(error)),
            ..
        } => {
            assert_eq!(error.kind, ErrorKind::NotFound);
        },
        other => panic!("expected lookup ProviderErr(NotFound) after 404, got {other:?}"),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn github_pr_lookup_validates_and_exposes_diff() {
    use omnifs_host::omnifs::provider::types::{
        BlobFetchRequest, BlobFetched, Callout, CalloutError, CalloutResult, ErrorKind,
        HttpRequest, HttpResponse, ReadFileBytes,
    };

    #[allow(clippy::needless_pass_by_value)]
    fn expect_fetch(response: StepOutcome) -> HttpRequest {
        let StepOutcome {
            result: None,
            callouts,
            ..
        } = &response
        else {
            panic!("expected callouts response, got {response:?}");
        };
        let [Callout::Fetch(request)] = callouts.as_slice() else {
            panic!("expected fetch callout, got {response:?}");
        };
        request.clone()
    }

    #[allow(clippy::needless_pass_by_value)]
    fn expect_blob_fetch(response: StepOutcome) -> BlobFetchRequest {
        let StepOutcome {
            result: None,
            callouts,
            ..
        } = &response
        else {
            panic!("expected callouts response, got {response:?}");
        };
        let [Callout::FetchBlob(request)] = callouts.as_slice() else {
            panic!("expected fetch-blob callout, got {response:?}");
        };
        request.clone()
    }

    let mut session = GithubProviderSession::new();

    let lookup_fetch =
        expect_fetch(session.lookup_child(70, "octocat/Hello-World/_prs/_open", "7"));
    assert!(
        lookup_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/pulls/7"),
        "unexpected PR lookup URL: {}",
        lookup_fetch.url
    );

    let lookup = session.resume(
        70,
        vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{
                "number": 7,
                "title": "Fix the thing",
                "body": "PR body",
                "state": "open",
                "user": {"login": "octocat"}
            }"#
            .to_vec(),
        })],
    );
    match lookup {
        StepOutcome {
            result: Some(OpResult::LookupChild(LookupChildResult::Entry(result))),
            effects,
            ..
        } => {
            let target = &result.target;
            assert_eq!(target.name, "7");
            assert!(matches!(target.kind, EntryKind::Directory));

            let names = project_paths(&effects);
            assert!(
                names.contains(&"octocat/Hello-World/_prs/_open/7/diff"),
                "lookup effects should project diff, got {names:?}"
            );
            assert!(
                names.contains(&"octocat/Hello-World/_prs/_open/7/comments"),
                "lookup effects should project comments, got {names:?}"
            );
        },
        other => panic!("expected validated PR lookup result, got {other:?}"),
    }

    let diff_fetch =
        expect_blob_fetch(session.read_file(70, "octocat/Hello-World/_prs/_open/7/diff"));
    assert!(
        diff_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/pulls/7"),
        "unexpected PR diff fetch URL: {}",
        diff_fetch.url
    );
    assert!(
        diff_fetch
            .headers
            .iter()
            .any(|h| h.name.eq_ignore_ascii_case("accept")
                && h.value == "application/vnd.github.diff"),
        "expected diff Accept header, got {:?}",
        diff_fetch.headers
    );

    let response = session.resume(
        70,
        vec![CalloutResult::BlobFetched(BlobFetched {
            blob: 1,
            size: 25,
            content_type: Some("application/octet-stream".to_string()),
            etag: None,
            status: 200,
            response_headers: Vec::new(),
        })],
    );

    match response {
        StepOutcome {
            result: Some(OpResult::ReadFile(file)),
            ..
        } => match &file.bytes {
            ReadFileBytes::Blob(blob) => assert_eq!(*blob, 1),
            ReadFileBytes::Inline(_) => panic!("expected blob-backed diff, got inline"),
        },
        other => panic!("expected PR diff file after read, got {other:?}"),
    }

    let retry = session.read_file(71, "octocat/Hello-World/_prs/_open/7/diff");
    assert!(
        retry.is_suspended(),
        "expected PR diff reread to refetch, got {retry:?}"
    );
    let response = session.resume(
        71,
        vec![CalloutResult::CalloutError(CalloutError {
            kind: ErrorKind::Network,
            message: "network down".to_string(),
            retryable: true,
        })],
    );
    match response {
        StepOutcome {
            result: Some(OpResult::Error(error)),
            ..
        } => {
            assert_eq!(error.kind, ErrorKind::Network);
        },
        other => panic!("expected Network error on refetch, got {other:?}"),
    }
}

#[test]
fn github_projected_resource_reads_return_all_fetched_siblings() {
    use omnifs_host::omnifs::provider::types::{Callout, CalloutResult, HttpRequest, HttpResponse};

    #[allow(clippy::needless_pass_by_value)]
    fn expect_fetch(response: StepOutcome) -> HttpRequest {
        let StepOutcome {
            result: None,
            callouts,
            ..
        } = &response
        else {
            panic!("expected callouts response, got {response:?}");
        };
        let [Callout::Fetch(request)] = callouts.as_slice() else {
            panic!("expected fetch callout, got {response:?}");
        };
        request.clone()
    }

    let mut session = GithubProviderSession::new();

    let pr_fetch = expect_fetch(session.read_file(72, "octocat/Hello-World/_prs/_open/7/title"));
    assert!(
        pr_fetch.url.ends_with("/repos/octocat/Hello-World/pulls/7"),
        "unexpected PR read URL: {}",
        pr_fetch.url
    );

    let pr_read = session.resume(
        72,
        vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{
                "number":7,
                "title":"PR title",
                "body":"PR body",
                "state":"open",
                "user":{"login":"octocat"}
            }"#
            .to_vec(),
        })],
    );
    match pr_read {
        StepOutcome {
            result: Some(OpResult::ReadFile(result)),
            effects,
            ..
        } => {
            assert_eq!(support::expect_inline(&result), b"PR title".to_vec());
            let project_paths = project_paths(&effects);
            assert_eq!(
                project_paths,
                vec![
                    "octocat/Hello-World/_prs/_open/7/body",
                    "octocat/Hello-World/_prs/_open/7/state",
                    "octocat/Hello-World/_prs/_open/7/user",
                ]
            );
        },
        other => panic!("expected PR file result with project effects, got {other:?}"),
    }

    let run_fetch =
        expect_fetch(session.read_file(73, "octocat/Hello-World/_actions/runs/123/status"));
    assert!(
        run_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/actions/runs/123"),
        "unexpected action run read URL: {}",
        run_fetch.url
    );

    let run_read = session.resume(
        73,
        vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{"id":123,"status":"completed","conclusion":"success"}"#.to_vec(),
        })],
    );
    match run_read {
        StepOutcome {
            result: Some(OpResult::ReadFile(result)),
            effects,
            ..
        } => {
            assert_eq!(support::expect_inline(&result), b"completed".to_vec());
            let project_paths = project_paths(&effects);
            assert_eq!(
                project_paths,
                vec![
                    "octocat/Hello-World/_actions/runs/123/conclusion",
                    "octocat/Hello-World/_actions/runs/123/log",
                ]
            );
            assert_eq!(
                project_file_stability(&effects, "octocat/Hello-World/_actions/runs/123/log"),
                Some(Stability::Mutable)
            );
        },
        other => panic!("expected action run file result with project effects, got {other:?}"),
    }
}

#[test]
fn github_provider_read_routes_dispatch_async_handlers() {
    for path in [
        "octocat/Hello-World/_issues/_open/1/title",
        "octocat/Hello-World/_prs/_open/1/diff",
        "octocat/Hello-World/_actions/runs/1/status",
    ] {
        let response = invoke_github_read_route(path);
        assert!(
            response.is_suspended(),
            "expected suspended provider step for {path}, got {response:?}"
        );
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn github_provider_resource_reads_do_not_fall_back_to_provider_cache() {
    use omnifs_host::omnifs::provider::types::{
        CalloutError, CalloutResult, ErrorKind, Header, HttpResponse,
    };

    struct Case {
        name: &'static str,
        path: &'static str,
        ok_headers: Vec<Header>,
        ok_body: &'static [u8],
        expected_content: &'static [u8],
    }

    // PR diff is covered separately by `github_pr_lookup_validates_and_exposes_diff`
    // because it dispatches a fetch-blob callout (and returns a blob-backed
    // ReadFileBytes) rather than an inline HttpResponse.
    let cases = [
        Case {
            name: "issue title",
            path: "octocat/Hello-World/_issues/_open/1/title",
            ok_headers: vec![Header {
                name: "etag".to_string(),
                value: "\"issue-1\"".to_string(),
            }],
            ok_body: br#"{
                "number": 1,
                "title": "Cached issue title",
                "body": "Body",
                "state": "open",
                "user": {"login": "octocat"}
            }"#,
            expected_content: b"Cached issue title",
        },
        Case {
            name: "action status",
            path: "octocat/Hello-World/_actions/runs/99/status",
            ok_headers: Vec::new(),
            ok_body: br#"{"id":99,"status":"completed","conclusion":"success"}"#,
            expected_content: b"completed",
        },
    ];

    let mut session = GithubProviderSession::new();
    let mut id = 1_u64;
    for case in &cases {
        let first = session.read_file(id, case.path);
        assert!(
            first.is_suspended(),
            "{name}: expected fetch callout on first read, got {first:?}",
            name = case.name
        );
        let cached = session.resume(
            id,
            vec![CalloutResult::HttpResponse(HttpResponse {
                status: 200,
                headers: case.ok_headers.clone(),
                body: case.ok_body.to_vec(),
            })],
        );
        match cached {
            StepOutcome {
                result: Some(OpResult::ReadFile(file)),
                ..
            } => {
                assert_eq!(
                    support::expect_inline(&file),
                    case.expected_content,
                    "{name}: unexpected cached content",
                    name = case.name
                );
            },
            other => panic!("{}: expected cached content, got {other:?}", case.name),
        }

        id += 1;
        let second = session.read_file(id, case.path);
        assert!(
            second.is_suspended(),
            "{name}: expected fetch callout on second read (no provider cache), got {second:?}",
            name = case.name
        );
        let error = session.resume(
            id,
            vec![CalloutResult::CalloutError(CalloutError {
                kind: ErrorKind::Network,
                message: "network down".to_string(),
                retryable: true,
            })],
        );
        match error {
            StepOutcome {
                result: Some(OpResult::Error(err)),
                ..
            } => {
                assert_eq!(
                    err.kind,
                    ErrorKind::Network,
                    "{}: wrong error kind",
                    case.name
                );
            },
            other => panic!(
                "{}: expected Network error on second read, got {other:?}",
                case.name
            ),
        }
        id += 1;
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn github_provider_comment_routes_refetch_and_reject_zero_index() {
    use omnifs_host::omnifs::provider::types::{Callout, CalloutError, CalloutResult, ErrorKind};

    fn ok_body(body: &[u8]) -> Vec<CalloutResult> {
        vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: body.to_vec(),
        })]
    }

    fn network_error() -> Vec<CalloutResult> {
        vec![CalloutResult::CalloutError(CalloutError {
            kind: ErrorKind::Network,
            message: "network down".to_string(),
            retryable: true,
        })]
    }

    fn expect_network_error_on_refetch(
        session: &mut GithubProviderSession,
        id: u64,
        dispatch: impl FnOnce(&mut GithubProviderSession, u64) -> StepOutcome,
    ) {
        let first = dispatch(session, id);
        assert!(
            first.is_suspended(),
            "expected fetch callout on refetch, got {first:?}"
        );
        match session.resume(id, network_error()) {
            StepOutcome {
                result: Some(OpResult::Error(error)),
                ..
            } => {
                assert_eq!(error.kind, ErrorKind::Network);
            },
            other => panic!("expected Network error on refetch, got {other:?}"),
        }
    }

    fn expect_not_found(response: StepOutcome) {
        match response {
            StepOutcome {
                result: Some(OpResult::Error(error)),
                ..
            } => {
                assert_eq!(error.kind, ErrorKind::NotFound);
            },
            other => panic!("expected NotFound error, got {other:?}"),
        }
    }

    fn expect_fetch_url(response: StepOutcome) -> String {
        let StepOutcome {
            result: None,
            callouts,
            ..
        } = response
        else {
            panic!("expected fetch callout, got {response:?}");
        };
        let [Callout::Fetch(request)] = callouts.as_slice() else {
            panic!("expected single fetch callout, got {callouts:?}");
        };
        request.url.clone()
    }

    let mut session = GithubProviderSession::new();

    // Issue comments surface through list_children.
    let issue_list_path = "octocat/Hello-World/_issues/_open/1/comments";
    let issue_first = session.list_children(50, issue_list_path);
    assert!(issue_first.is_suspended());
    match session.resume(
        50,
        ok_body(br#"[{"user":{"login":"octocat"},"body":"first issue comment"}]"#),
    ) {
        StepOutcome {
            result: Some(OpResult::ListChildren(ListChildrenResult::Entries(listing))),
            ..
        } => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert_eq!(names, vec!["1"]);
            let EntryKind::File(file) = &listing.entries[0].kind else {
                panic!("expected comment entry to be a file");
            };
            assert_eq!(file.attrs.stability, Stability::Mutable);
        },
        other => panic!("expected issue comment listing, got {other:?}"),
    }
    expect_network_error_on_refetch(&mut session, 51, |s, id| {
        s.list_children(id, issue_list_path)
    });
    expect_not_found(session.read_file(52, "octocat/Hello-World/_issues/_open/1/comments/0"));

    let issue_page_two_url =
        expect_fetch_url(session.read_file(56, "octocat/Hello-World/_issues/_open/1/comments/101"));
    assert!(
        issue_page_two_url.contains("issues/1/comments?per_page=100&page=2"),
        "expected second-page issue comment fetch, got {issue_page_two_url}"
    );
    match session.resume(
        56,
        ok_body(br#"[{"user":{"login":"octocat"},"body":"page two issue comment"}]"#),
    ) {
        StepOutcome {
            result: Some(OpResult::ReadFile(file)),
            ..
        } => {
            assert_eq!(file.attrs.stability, Stability::Mutable);
            assert_eq!(
                support::expect_inline(&file),
                b"octocat:\npage two issue comment\n"
            );
        },
        other => panic!("expected issue comment page-two content, got {other:?}"),
    }

    // PR comments surface through read_file at a specific index.
    let pr_read_path = "octocat/Hello-World/_prs/_open/7/comments/1";
    let pr_first = session.read_file(53, pr_read_path);
    assert!(pr_first.is_suspended());
    match session.resume(
        53,
        ok_body(br#"[{"user":{"login":"hubot"},"body":"first pr comment"}]"#),
    ) {
        StepOutcome {
            result: Some(OpResult::ReadFile(file)),
            ..
        } => {
            assert_eq!(file.attrs.stability, Stability::Mutable);
            assert_eq!(support::expect_inline(&file), b"hubot:\nfirst pr comment\n");
        },
        other => panic!("expected PR comment content, got {other:?}"),
    }
    expect_network_error_on_refetch(&mut session, 54, |s, id| s.read_file(id, pr_read_path));
    expect_not_found(session.read_file(55, "octocat/Hello-World/_prs/_open/7/comments/0"));

    let pr_page_two_url =
        expect_fetch_url(session.read_file(57, "octocat/Hello-World/_prs/_open/7/comments/101"));
    assert!(
        pr_page_two_url.contains("issues/7/comments?per_page=100&page=2"),
        "expected second-page PR comment fetch, got {pr_page_two_url}"
    );
    match session.resume(
        57,
        ok_body(br#"[{"user":{"login":"hubot"},"body":"page two pr comment"}]"#),
    ) {
        StepOutcome {
            result: Some(OpResult::ReadFile(file)),
            ..
        } => {
            assert_eq!(file.attrs.stability, Stability::Mutable);
            assert_eq!(
                support::expect_inline(&file),
                b"hubot:\npage two pr comment\n"
            );
        },
        other => panic!("expected PR comment page-two content, got {other:?}"),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn github_provider_paginates_issue_and_pr_results_in_parallel() {
    use omnifs_host::omnifs::provider::types::{
        Callout, CalloutResult, Header, HttpRequest, HttpResponse,
    };

    #[allow(clippy::needless_pass_by_value)]
    fn expect_fetch(response: StepOutcome) -> HttpRequest {
        let StepOutcome {
            result: None,
            callouts,
            ..
        } = &response
        else {
            panic!("expected callouts response, got {response:?}");
        };
        let [Callout::Fetch(request)] = callouts.as_slice() else {
            panic!("expected fetch callout, got {response:?}");
        };
        request.clone()
    }

    fn page_items(first_number: u64) -> String {
        (first_number..first_number + 100)
            .map(|number| {
                format!(
                    r#"{{
                        "number": {number},
                        "title": "page item",
                        "body": "text",
                        "state": "open",
                        "user": {{"login": "octocat"}}
                    }}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",")
    }

    fn search_page(total_count: u64, first_number: u64) -> CalloutResult {
        let body = format!(
            r#"{{"total_count":{total_count},"items":[{}]}}"#,
            page_items(first_number)
        );
        CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: body.into_bytes(),
        })
    }

    fn rest_page(first_number: u64) -> CalloutResult {
        let body = format!("[{}]", page_items(first_number));
        CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: vec![Header {
                name: "etag".to_string(),
                value: "\"page\"".to_string(),
            }],
            body: body.into_bytes(),
        })
    }

    fn assert_page_fetches(callouts: &[Callout], expected: std::ops::RangeInclusive<u64>) {
        let expected_pages: Vec<u64> = expected.collect();
        assert_eq!(callouts.len(), expected_pages.len());
        for (callout, page) in callouts.iter().zip(expected_pages) {
            let page_suffix = format!("&page={page}");
            let page_middle = format!("{page_suffix}&");
            assert!(
                matches!(callout, Callout::Fetch(req)
                    if req.url.ends_with(&page_suffix) || req.url.contains(&page_middle)),
                "expected page {page} fetch, got {callout:?}"
            );
        }
    }

    let mut session = GithubProviderSession::new();

    let first_issue_page =
        expect_fetch(session.list_children(20, "octocat/Hello-World/_issues/_all"));
    assert!(
        first_issue_page.url.ends_with(
            "/search/issues?q=repo:octocat/Hello-World&sort=created&order=desc&per_page=100"
        ),
        "unexpected issue list URL: {}",
        first_issue_page.url
    );
    let issue_parallel = session.resume(20, vec![search_page(1500, 1)]);
    let StepOutcome {
        result: None,
        callouts,
        ..
    } = &issue_parallel
    else {
        panic!("expected parallel issue page fetches, got {issue_parallel:?}");
    };
    assert_page_fetches(callouts, 2..=10);
    let issue_pages = (2..=10).map(|page| rest_page(page * 100)).collect();
    let final_response = session.resume(20, issue_pages);
    match final_response {
        StepOutcome {
            result: Some(OpResult::ListChildren(ListChildrenResult::Entries(listing))),
            ..
        } => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert!(names.contains(&"1"));
            assert!(names.contains(&"200"));
            assert!(names.contains(&"1099"));
            assert!(
                !listing.exhaustive,
                "issue listing should remain partial after hitting the search-API page cap"
            );
        },
        other => panic!("expected paginated issue listing, got {other:?}"),
    }

    let first_pr_page = expect_fetch(session.list_children(21, "octocat/Hello-World/_prs/_all"));
    assert!(first_pr_page.url.ends_with(
        "/search/issues?q=repo:octocat/Hello-World+is:pr&sort=created&order=desc&per_page=100"
    ));
    let pr_parallel = session.resume(21, vec![search_page(1500, 7)]);
    let StepOutcome {
        result: None,
        callouts,
        ..
    } = &pr_parallel
    else {
        panic!("expected parallel PR page fetches, got {pr_parallel:?}");
    };
    assert_page_fetches(callouts, 2..=10);
    let pr_pages = (2..=10).map(|page| rest_page(page * 100 + 7)).collect();
    let final_response = session.resume(21, pr_pages);
    match final_response {
        StepOutcome {
            result: Some(OpResult::ListChildren(ListChildrenResult::Entries(listing))),
            ..
        } => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert!(names.contains(&"7"));
            assert!(names.contains(&"207"));
            assert!(names.contains(&"1106"));
            assert!(
                !listing.exhaustive,
                "PR listing should remain partial after hitting the search-API page cap"
            );
        },
        other => panic!("expected paginated PR listing, got {other:?}"),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn github_provider_lookup_owner_validates_and_owner_listing_classifies_with_org_fallback() {
    use omnifs_host::omnifs::provider::types::{
        Callout, CalloutResult, Header, HttpRequest, HttpResponse,
    };

    #[allow(clippy::needless_pass_by_value)]
    fn expect_fetch(response: StepOutcome) -> HttpRequest {
        let StepOutcome {
            result: None,
            callouts,
            ..
        } = &response
        else {
            panic!("expected callouts response, got {response:?}");
        };
        let [Callout::Fetch(request)] = callouts.as_slice() else {
            panic!("expected fetch callout, got {response:?}");
        };
        request.clone()
    }

    let mut session = GithubProviderSession::new();

    let first = expect_fetch(session.lookup_child(30, "", "openai"));
    assert!(
        first.url.ends_with("/users/openai"),
        "expected user profile lookup first, got {}",
        first.url
    );

    let second = expect_fetch(session.resume(
        30,
        vec![CalloutResult::HttpResponse(HttpResponse {
            status: 404,
            headers: vec![Header {
                name: "etag".to_string(),
                value: "\"miss\"".to_string(),
            }],
            body: Vec::new(),
        })],
    ));
    assert!(
        second.url.ends_with("/orgs/openai"),
        "expected org profile fallback, got {}",
        second.url
    );

    let repos_fetch = expect_fetch(session.resume(
        30,
        vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{
                "login": "openai",
                "public_repos": 42
            }"#
            .to_vec(),
        })],
    ));
    assert!(
        repos_fetch
            .url
            .ends_with("/orgs/openai/repos?per_page=100&sort=updated&page=1"),
        "expected repo listing fetch after owner classification, got {}",
        repos_fetch.url
    );

    let lookup = session.resume(
        30,
        vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"[{"name":"api"}]"#.to_vec(),
        })],
    );
    match lookup {
        StepOutcome {
            result: Some(OpResult::LookupChild(LookupChildResult::Entry(result))),
            effects,
            ..
        } => {
            let entry = &result.target;
            assert_eq!(entry.name, "openai");
            assert!(matches!(entry.kind, EntryKind::Directory));
            let names = project_paths(&effects);
            assert!(
                names.contains(&"openai/api"),
                "expected repo lookup projection after owner classification, got {names:?}"
            );
        },
        other => panic!("expected owner lookup result, got {other:?}"),
    }

    // Root is not enumerable; should always return empty, regardless
    // of which owners have been resolved in prior calls.
    let root_listing = session.list_children(32, "");
    match root_listing {
        StepOutcome {
            result: Some(OpResult::ListChildren(ListChildrenResult::Entries(listing))),
            ..
        } => {
            assert!(
                listing.entries.is_empty(),
                "root should be empty, got {:?}",
                listing.entries
            );
        },
        other => panic!("expected empty root listing, got {other:?}"),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn github_provider_polls_events_and_invalidates_caches() {
    use omnifs_host::omnifs::provider::types::{
        ActivePathSet, Callout, CalloutError, CalloutResult, ErrorKind, Header, HttpRequest,
        HttpResponse,
    };

    #[allow(clippy::needless_pass_by_value)]
    fn expect_fetch(response: StepOutcome) -> HttpRequest {
        let StepOutcome {
            result: None,
            callouts,
            ..
        } = &response
        else {
            panic!("expected callouts response, got {response:?}");
        };
        let [Callout::Fetch(request)] = callouts.as_slice() else {
            panic!("expected fetch callout, got {response:?}");
        };
        request.clone()
    }

    fn expect_callouts(response: StepOutcome) -> Vec<Callout> {
        let StepOutcome {
            result: None,
            callouts,
            ..
        } = response
        else {
            panic!("expected callouts response, got {response:?}");
        };
        callouts
    }

    fn repo_active_path(owner: &str, repo: &str) -> ActivePathSet {
        ActivePathSet {
            mount_id: "/{owner}/{repo}".to_string(),
            mount_name: "Repo".to_string(),
            paths: vec![format!("/{owner}/{repo}")],
        }
    }

    let mut session = GithubProviderSession::new();
    let issue_path = "octocat/Hello-World/_issues/_open/1/title";

    let issue_fetch = expect_fetch(session.read_file(40, issue_path));
    assert!(
        issue_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/issues/1"),
        "unexpected issue fetch URL: {}",
        issue_fetch.url
    );
    let issue_cached = session.resume(
        40,
        vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: vec![Header {
                name: "etag".to_string(),
                value: "\"issue-1\"".to_string(),
            }],
            body: br#"{
                "number": 1,
                "title": "Cached issue title",
                "body": "Body",
                "state": "open",
                "user": {"login": "octocat"}
            }"#
            .to_vec(),
        })],
    );
    match issue_cached {
        StepOutcome {
            result: Some(OpResult::ReadFile(file)),
            ..
        } => {
            assert_eq!(support::expect_inline(&file), b"Cached issue title");
        },
        other => panic!("expected cached issue file content, got {other:?}"),
    }

    let first_tick = expect_callouts(
        session.timer_tick_with_paths(41, vec![repo_active_path("octocat", "Hello-World")]),
    );
    assert_eq!(
        first_tick.len(),
        1,
        "unexpected first tick callouts: {first_tick:?}"
    );
    let Callout::Fetch(first_events_request) = &first_tick[0] else {
        panic!("expected first tick fetch callout, got {:?}", first_tick[0]);
    };
    assert!(
        first_events_request
            .url
            .ends_with("/repos/octocat/Hello-World/events?per_page=30"),
        "unexpected events URL: {}",
        first_events_request.url
    );

    // Invalidations now live on the event-outcome terminal rather than
    // fire-and-forget callouts.
    let first_tick_done = session.resume(
        41,
        vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: vec![Header {
                name: "etag".to_string(),
                value: "\"events-1\"".to_string(),
            }],
            body: br#"[{"type":"IssuesEvent"}]"#.to_vec(),
        })],
    );
    assert!(
        first_tick_done.callouts.is_empty(),
        "event-outcome terminals should not carry callouts, got {:?}",
        first_tick_done.callouts
    );
    match &first_tick_done.result {
        Some(OpResult::OnEvent) => {
            let prefixes = invalidate_prefixes(&first_tick_done.effects);
            assert_eq!(
                prefixes,
                vec!["octocat/Hello-World/_issues"],
                "unexpected invalidate_prefixes: {prefixes:?}"
            );
        },
        other => panic!("expected Event terminal with invalidations, got {other:?}"),
    }

    let issue_refetch = expect_fetch(session.read_file(42, issue_path));
    assert!(
        issue_refetch
            .url
            .ends_with("/repos/octocat/Hello-World/issues/1"),
        "unexpected issue refetch URL: {}",
        issue_refetch.url
    );
    let stale_after_invalidation = session.resume(
        42,
        vec![CalloutResult::CalloutError(CalloutError {
            kind: ErrorKind::Network,
            message: "network down".to_string(),
            retryable: true,
        })],
    );
    assert!(
        matches!(
            stale_after_invalidation,
            StepOutcome {
                result: Some(OpResult::Error(_)),
                ..
            }
        ),
        "expected invalidated cache miss, got {stale_after_invalidation:?}"
    );

    let second_tick = expect_callouts(
        session.timer_tick_with_paths(43, vec![repo_active_path("octocat", "Hello-World")]),
    );
    assert_eq!(
        second_tick.len(),
        1,
        "unexpected second tick callouts: {second_tick:?}"
    );
    let Callout::Fetch(second_events_request) = &second_tick[0] else {
        panic!(
            "expected second tick fetch callout, got {:?}",
            second_tick[0]
        );
    };
    assert!(
        second_events_request.headers.iter().any(|header| {
            header.name.eq_ignore_ascii_case("if-none-match") && header.value == "\"events-1\""
        }),
        "missing If-None-Match header on second poll: {:?}",
        second_events_request.headers
    );
    let second_tick_done = session.resume(
        43,
        vec![CalloutResult::HttpResponse(HttpResponse {
            status: 304,
            headers: vec![Header {
                name: "etag".to_string(),
                value: "\"events-1\"".to_string(),
            }],
            body: Vec::new(),
        })],
    );
    assert!(
        matches!(
            second_tick_done,
            StepOutcome {
                result: Some(OpResult::OnEvent),
                ..
            }
        ),
        "expected second timer tick event terminal, got {second_tick_done:?}"
    );
}

#[test]
fn github_provider_list_routes_preserve_typed_http_errors() {
    use omnifs_host::omnifs::provider::types::{
        Callout, CalloutResult, ErrorKind, Header, HttpRequest, HttpResponse,
    };

    #[allow(clippy::needless_pass_by_value)]
    fn expect_fetch(response: StepOutcome) -> HttpRequest {
        let StepOutcome {
            result: None,
            callouts,
            ..
        } = &response
        else {
            panic!("expected callouts response, got {response:?}");
        };
        let [Callout::Fetch(request)] = callouts.as_slice() else {
            panic!("expected fetch callout, got {response:?}");
        };
        request.clone()
    }

    fn denied_page() -> Vec<CalloutResult> {
        vec![CalloutResult::HttpResponse(HttpResponse {
            status: 403,
            headers: vec![Header {
                name: "etag".to_string(),
                value: "\"denied\"".to_string(),
            }],
            body: br#"{"message":"forbidden"}"#.to_vec(),
        })]
    }

    fn expect_denied(response: StepOutcome) {
        let StepOutcome {
            result: Some(OpResult::Error(error)),
            ..
        } = response
        else {
            panic!("expected provider error result, got {response:?}");
        };
        assert_eq!(error.kind, ErrorKind::Denied);
    }

    let cases = [
        (
            "issues",
            "octocat/Hello-World/_issues/_all",
            "/search/issues?q=repo:octocat/Hello-World&sort=created&order=desc&per_page=100",
        ),
        (
            "prs",
            "octocat/Hello-World/_prs/_all",
            "/search/issues?q=repo:octocat/Hello-World+is:pr&sort=created&order=desc&per_page=100",
        ),
        (
            "actions",
            "octocat/Hello-World/_actions/runs",
            "/repos/octocat/Hello-World/actions/runs?per_page=30",
        ),
    ];

    let mut session = GithubProviderSession::new();
    for (index, (kind, path, suffix)) in cases.into_iter().enumerate() {
        let id = 50 + index as u64;
        let fetch = expect_fetch(session.list_children(id, path));
        assert!(
            fetch.url.ends_with(suffix),
            "{kind}: unexpected URL {}",
            fetch.url
        );
        expect_denied(session.resume(id, denied_page()));
    }
}
