# Target source tree

Status: companion to `docs/future/bluesky-rewrite.md`. The full post-rework source layout: crates, files, key symbols, responsibilities. `←` marks provenance (where today's code moves from); files without a marker stay where they are. Line counts are approximate masses for orientation, not budgets. This is the placement rule made concrete: new code goes where this tree says its responsibility lives.

## Dependency DAG

```
                 ┌────────────────────── guest wall ──────────────────────┐
                 │  providers/*  →  omnifs-sdk  →  omnifs-core, omnifs-wit │
                 │                  (macros: omnifs-sdk-macros)            │
                 └──────────────────────────────────────────────────────────┘

leaves      omnifs-core   omnifs-wit   omnifs-caps   omnifs-api   omnifs-mtab
                 │             │            │             │            │
state       omnifs-workspace ──┼────────────┘             │            │
                 │             │                          │            │
auth        omnifs-auth       │                          │            │
                 │             │                          │            │
engine      omnifs-engine ────┘   (also: caps, api-events, workspace, auth)
                 │
frontends   omnifs-fuse (linux)      omnifs-nfs          (both: engine, core, mtab)
                 │                        │
server      omnifs-daemon  (api, engine, auth, workspace, frontends, mtab)
                 │
client      omnifs-cli     (api, auth, workspace, caps, core, mtab; features: daemon, nfs)

tools       omnifs-embed-metadata (sdk, workspace)      omnifs-itest (engine, workspace, wit)
```

Five walls, restated: guest (sdk's dep list), trust (wit), protocol (fuse/nfs), control wire (api between daemon and cli), state-without-wasmtime (workspace below engine).

## Mass before and after

| Target crate | ~lines | Composed from (today) |
|---|---|---|
| omnifs-core | 600 | core 2,179 minus view/auth/mount/provider modules |
| omnifs-wit | 120 | unchanged |
| omnifs-caps | 950 | unchanged |
| omnifs-api | 1,400 | api 225 + inspector 804 + v2 DTOs |
| omnifs-mtab | 350 | new; replaces ~500 lines of duplication across nfs/daemon/cli |
| omnifs-sdk (+macros) | 14,500 | unchanged mass, restructured internally |
| omnifs-workspace | 7,000 | mount 1,717 + provider 3,340 + creds 740 + home 200 + moved core types |
| omnifs-auth | 3,500 | auth 1,963 + the state machine leaving host + the service |
| omnifs-engine | 20,000 | host 12,985 + tree 3,295 + cache 2,199 + core view ~1,000 |
| omnifs-fuse | 3,000 | roughly unchanged; sheds duplicated tables to engine::render |
| omnifs-nfs | 8,500 | sheds mount-table/unmount code to mtab, tables to engine::render |
| omnifs-daemon | 2,000 | daemon 1,142 + the v2 route handlers |
| omnifs-cli | 12,000 | cli 14,323 minus session/teardown-dup/readiness-arithmetic/launch-guessing |

---

## Workspace root

```
omnifs/
├── Cargo.toml                workspace; default-members exclude guest crates (footgun stands)
├── AGENTS.md / CLAUDE.md     updated orientation: the 16-crate table
├── Dockerfile                dev runtime image (consumes target/omnifs-provider-store)
├── justfile, just/*.just
├── scripts/
│   ├── dev.ts                contributor dev home; writes specs/creds THROUGH the CLI, no hand-rolled writers
│   └── ci/                   build-runtime-image.sh, check-doc-links.sh, guest-path-literal grep, engine-visibility assertion
├── docs/                     contracts/, architecture/, future/, internal/
├── crates/                   16 crates below
└── providers/                arxiv, db, dns, docker, github, kubernetes, linear, oura, test + DESIGN.md
```

---

## Tier 0: leaves

### `omnifs-core` — what a provider guest may know

```
src/
├── lib.rs
├── path.rs             Path, Segment, PathError — validated absolute UTF-8 protocol paths; join/parent/prefix
└── content_type.rs     ContentType — extension ↔ MIME inference
```

Sheds `view.rs` → engine, `auth.rs` → workspace, `mount.rs` → workspace, `provider.rs` → workspace. What remains is exactly the set every guest import actually uses.

### `omnifs-wit` — the provider contract

```
wit/provider.wit        callout families (http, git, blob, archive), effects (canonical-store, fs-write,
                        invalidation), namespace/notify exports; capability-need splits into
                        access-need + resource-limit in the one contract-touching slice
src/lib.rs              bindgen host bindings + small ergonomic impls on generated types
```

### `omnifs-caps` — the capability language

```
src/
├── lib.rs              re-exports; doc: "enforcement lives in the engine, not here"
├── model.rs            AccessNeed (was Need), ResourceLimit (post-split), Grant<T> (tagged), Grants
├── matching.rs         domain / unix-socket / path matchers — one owner so the start-time check
│                       and the runtime allowlist cannot drift
├── resolve.rs          dynamic grant resolution (UnixSocket, PreopenedPath — the only dynamic kinds)
├── allowlist.rs        Allowlist — the runtime allow/deny decision table
└── check.rs            Grants::satisfies — start-time satisfiability, fail-fast
```

### `omnifs-api` — the control-plane wire contract

```
src/
├── lib.rs              API_MAJOR = 2, API_MINOR, DEFAULT_PORT, default_listen_addr()  ← the 4 hand-built SocketAddrs
├── status.rs           DaemonStatus, DaemonHealth, SubsystemHealth, HealthState,
│                       DaemonBackend { Native{pid}, Docker{container_name, image} }   ← launch.json's exclusive facts
│                       FrontendInfo { fs_type: FsType (enum, was String) }
├── mounts.rs           MountSummary { mount, provider_name, provider_id: hash, health, auth_state },
│                       MountDetail (grants, scheme, pinned artifact, reconcile history),
│                       MountReport, UpgradeDelta (what PUT approval is checked against),
│                       ReconcileReport, StopReport
├── credentials.rs      CredentialStatus — health, expiry timestamp, scopes; NEVER token material
├── providers.rs        ProviderSummary — installed/available per name
├── error.rs            ApiError { code: ErrorCode, message, detail? } — every non-2xx body
└── events/                                                             ← omnifs-inspector, whole crate
    ├── envelope.rs     InspectorRecord
    ├── event.rs        InspectorEvent — fuse/provider/callout/subtree/clone/cache stages, elapsed_us
    ├── outcome.rs      InspectorOutcome, OutcomeFields
    ├── redact.rs       redaction rules (strip all query params; auth headers)
    └── wire.rs         InspectorRecord::{parse, parse_line, to_json}    (methods, were free fns)
openapi/daemon.json     generated by `just openapi`; byte-parity test in the daemon
```

### `omnifs-mtab` — reading and unwinding OS mount state

```
src/
├── lib.rs
├── proc_mounts.rs      MountEntry, parse_proc_mounts(), decode_mount_field()
│                       ← the byte-identical copies in daemon/proc_mounts.rs and nfs/mount.rs
├── state.rs            NfsMountState (one VERSION const), NfsMountState::read_all, StateFile::write
│                       ← nfs/mount.rs + the hand-synced constant in cli/host_teardown.rs
└── unmount.rs          UnmountCommand::{graceful, forced} — per-platform program+args builders
                        ← the forks in nfs/mount.rs and cli/host_teardown.rs
```

---

## Guest branch

### `omnifs-sdk-macros` — provider authoring macros

```
src/
├── lib.rs
├── provider_macro.rs   #[provider] — ProviderArgs::expand(); emits Provider::METADATA, the native
│                       provider_metadata() accessor (non-wasm), wasm namespace/notify exports
├── config_macro.rs     #[config] — ConfigArgs::expand(); HostResource bindings stay per-field
├── object_macro.rs     #[object] — ObjectArgs::expand()
├── endpoint_macro.rs   #[endpoint] — EndpointArgs::expand()  (was one 174-line inline function)
├── captures_macro.rs   path-captures derive (shared generic-arg extraction helper, was 4 copies)
└── path_segment_macro.rs
```

One shape everywhere: parse into `XArgs`, call `XArgs::expand()`.

### `omnifs-sdk` — the provider authoring surface

```
src/
├── lib.rs, prelude.rs
├── object.rs           Object, Key, LogicalId, Facet, ObjectKind (typed newtype used everywhere,
│                       including seal-time comparisons), Canonical — the one name for verbatim
│                       upstream bytes; role-named projections elsewhere
├── collection.rs       Collection<T, C> — THE listing surface; Collection::into_dir_projection
│                       (DirProjection off the public surface; Linear on Collection<Issue>)
├── router/
│   ├── mod.rs          Router — register in start, seal() validates once, read-only dispatch after
│   ├── register.rs     two-stage registration: declarations until seal, then plain
│   │                   Rc<ResolvedChildView> (the Rc<RefCell<Option<Rc<..>>>> cell and the
│   │                   string-matched parallel lists are gone)
│   ├── object/                                          ← router/object.rs (2,049 lines) split
│   │   ├── spec.rs     ObjectSpec, ObjectHandle::{new, new_file}; faces: canonical,
│   │   │               representation (render-table), computed (was "derive")
│   │   ├── dispatch.rs ObjectRoute::{lookup, list, read}
│   │   └── serve.rs    ServeCtx with impl: serve_warm/serve_fresh/serve_from_canonical/serve_computed
│   ├── pattern.rs      PatternSegment::{signature, overlaps}; typed PatternError
│   │                   (the Result<_, String> shadow channel is gone)
│   └── dispatch/       route_shape.rs, static_shape.rs — precedence, capture validation
├── endpoint.rs         Endpoint, Load::from_response
├── projection.rs       FileProjection, FileProjBuilder (type-state)
├── file_attrs.rs       provider-declared attrs, WIT-shaped (no engine::view dependency — the wall)
├── handler.rs          pagination / ranged-read / TreeRef handoff plumbing
├── http.rs, git.rs,
│   archives.rs, blob.rs  callout futures (shared extractor fn pointers, no per-site closures)
├── repr.rs             render tables; pretty_json (helpers.rs dissolved; err() was redundant with From)
└── error.rs            ProviderError, ProviderErrorKind
```

Dependencies: core, wit, macros. Nothing else, ever; CI asserts it.

### `providers/*`

Unchanged shape: one `#[omnifs_sdk::provider]` impl each, `dev/mount.json` fixtures regenerated from manifests (not hand-authored). github, linear, docker are the tracers that gate every authoring-surface slice.

---

## `omnifs-workspace` — every byte under OMNIFS_HOME

```
src/
├── lib.rs
├── layout.rs           WorkspaceLayout — sole OMNIFS_HOME resolution                ← omnifs-home
│                       wasm_cache_dir (one copy; host's duplicate dies),
│                       resolve_mount_point (env-honoring; setup preview + daemon share it)
├── ids.rs              ProviderName, ProviderId([u8;32]), ProviderVersion,
│                       ProviderMeta, ProviderRef                                    ← core/provider.rs
│                       (ToSchema derives behind a `utoipa` feature — gated surface)
├── authn/              the auth vocabulary                    ← core/auth.rs + provider/auth_wire.rs
│   ├── ids.rs          SchemeId, AccountId, AuthKind,
│   │                   CredentialId + CredentialId::for_mount(provider, &spec::Auth)
│   │                   — THE single key derivation (CLI and engine both call it;
│   │                   host's hand-copied DEFAULT_ACCOUNT dies)
│   ├── scheme.rs       AuthManifest, AuthScheme, StaticTokenScheme { ambient_sources, validation },
│   │                   OauthScheme { device_poll_compat: Rfc8628 | ErrorInOkBody, .. },
│   │                   OAuthFlow, TokenEndpointAuthMethod, SchemeGuidance
│   │                   — every struct deny_unknown_fields (today only TokenValidation is)
│   └── resolve.rs      scheme resolution by key / sole-scheme inference
├── provider/                                                            ← omnifs-provider
│   ├── manifest.rs     ProviderManifest — the single wire type; validation now includes
│   │                   inject-domains ⊆ declared domain needs
│   ├── config.rs       ConfigMetadata, ConfigField, ConfigType, HostResourceBinding; ::defaults()
│   ├── sections.rs     omnifs.provider-metadata.v1 read/embed (never instantiates wasm)
│   ├── store.rs        content-addressed artifact store; Index/IndexEntry (now strict)
│   ├── catalog.rs      Catalog::{get, installable, latest_by_name} — feeds GET /v1/providers
│   ├── wasm.rs         ProviderWasm; Artifact — the ONLY Artifact (host's wrapper deleted)
│   ├── authoring.rs    non-wasm builder DSL used inside #[provider] bodies
│   └── records.rs      route-manifest records; rec.encode() methods
├── mounts/                                                              ← omnifs-mount
│   ├── spec.rs         Spec (deny_unknown_fields — gated), spec::Auth { StaticToken, OAuth },
│   │                   mount::Name (filename == mount invariant)        ← core/mount.rs
│   ├── registry.rs     Registry — SOLE spec owner; load/put/remove; atomic writes;
│   │                   NEW: advisory file lock (daemon becomes a co-writer in API v2)
│   ├── inherit.rs      Spec::apply_provider_metadata — the ONLY manifest-defaults fold;
│   │                   init, upgrade, and itest all call it (the three CLI shadows die)
│   ├── materialize.rs  Spec::materialize → MaterializedMount — grant sufficiency + preopen rewrite
│   └── upgrade.rs      UpgradePlan::diff — binds the FULL surface: endpoints, scopes,
│                       inject domain/header, flow kind, config field types, capability grants
├── creds/                                                               ← omnifs-creds
│   ├── entry.rs        CredentialEntry, Refreshability
│   ├── store.rs        CredentialStore trait, StoreError
│   ├── file.rs         FileStore — credentials.json, 0600 file / 0700 dir, atomic, advisory-locked
│   └── memory.rs       MemoryStore (tests)
├── launch.rs           LaunchRecord (launch.json) — now a CACHE of daemon-reported identity
│                       for the daemon-down case; the guess-the-container fallback is deleted
└── io.rs               the ONE atomic-write helper (atomic_write_file crate) — mounts, config,
                        launch record, creds all route through it
```

## `omnifs-auth` — credential lifecycle

```
src/
├── lib.rs
├── service.rs          CredentialService — per-credential state table; authorization(),
│                       report_rejected(), health(), reload(), store(), revoke_and_delete(),
│                       spawn_refresh_loop() (jittered, single-flight, wakes at earliest
│                       expiry - window)
├── health.rs           CredentialHealth { Ready, ExpiringSoon, Expired, RefreshFailed,
│                       NeedsConsent, Missing, StaticUnvalidated }, CredentialStatus;
│                       the ONE expiry function + REFRESH_WINDOW (the three copies die)
├── client.rs           OAuthClient — flow façade                       (1,110-line file split)
├── flows/
│   ├── loopback.rs     LoopbackEndpoint — bind rules, CSRF, ephemeral port
│   ├── device.rs       device flow; DevicePollCompat handling (the GitHub shim, now declared
│   │                   by the provider's scheme instead of hardcoded vendor knowledge)
│   ├── manual.rs       manual-code flow
│   └── implicit.rs     client-side token flow + fragment-capture page
├── callback.rs         LoopbackCallback::parse, ClientSideTokenCallback::parse
├── request.rs          OAuthRequest::from_mount_config; token_endpoint_secret(&self) (zero-arg)
├── validate.rs         TokenValidation probe runner — at token entry AND on the slow cadence
│                       for static-token health
└── error.rs            AuthError — consolidated variants; ONE generic RequestTokenError mapper
                        (was three copy-pasted From impls)
```

Depends on workspace + core. The old inverted edges (auth → mount, auth → provider) cannot exist: those crates are gone.

## `omnifs-engine` — the trusted runtime

```
src/
├── lib.rs              THE CURATED PUBLIC SURFACE: Engine, MountRuntimes, ServingContext, Tree,
│                       render::*, view::*, TreeError/TreeErrorKind, health, event stream.
│                       Everything else pub(crate). A CI assertion pins this list.
├── runtime/
│   ├── mod.rs          Runtime::build — the trust path: reads the pinned manifest from artifact
│   │                   bytes, resolves preopens, wires callouts+capability+auth-inject;
│   │                   validates credential health at mount start (new)
│   ├── instance.rs     Instance — per-instance driver thread, Store::run_concurrent over
│   │                   FuturesUnordered, call_concurrent dispatch, Command channel
│   ├── wasm.rs         component_engine(), compiler strategy (Winch/Cranelift via env)
│   ├── wasi.rs         HostState, WIT host-import impls (the literal import surface)
│   └── registry.rs     MountRuntimes (was ProviderRegistry) — reconcile pass
│                       (plan serial → build parallel → publish → remove-stale),
│                       BLAKE3 fingerprints, refresh timers,
│                       NEW: consults UpgradePlan::diff and refuses unapproved widening swaps
├── ops/
│   ├── op.rs           Op, LiveOpDescriptor
│   ├── lifecycle.rs    run_op / finish_provider_return — captures op_gen (the write fence)
│   ├── validate.rs     validate_return — structural checks before any state mutation
│   └── namespace.rs    Namespace — coalescing entry point; run_op_expect (the five duplicated
│                       dispatch-matches collapse); returns DOMAIN outcomes for all five ops:
│                       LookupOutcome, ListOutcome, ReadOutcome, OpenOutcome, ChunkOutcome
│                       — wit types stop here, permanently
├── callouts/
│   ├── mod.rs          CalloutHost::dispatch — per-family routing, tracing spans,
│   │                   spawn_blocking for git/blob.read/archive
│   ├── http.rs         HttpStack — allowlist check → CredentialService::authorization →
│   │                   send → report_rejected-gated single retry
│   ├── git.rs          GitExecutor;  cloner.rs  GitCloner (per-repo locks, stderr capture)
│   ├── blob.rs         BlobExecutor — streaming with byte caps, BlobLimits
│   ├── archive.rs      ArchiveExecutor + ArchiveFormat::extract — the trust boundary for
│   │                   archive bytes (sanitize_path, resource caps)
│   └── wit_convert.rs  FromWit / ToWit extension traits — pub(crate)   (was pub wit_protocol.rs)
├── auth_inject.rs      per-mount domain → CredentialId map + header composition; all POLICY
│                       delegated to omnifs-auth; fail-closed on Missing/NeedsConsent
├── capability.rs       CapabilityChecker — pub(crate); gates every callout
├── effects/
│   ├── apply.rs        EffectApplier (was Materializer) — split per effect kind: canonical
│   │                   batch (id-conflict filtered) → fs writes → dirent merge → invalidations
│   │                   (which DELETE durable state, not just fence)
│   └── invalidation.rs InvalidationState — drain queues frontends consume
├── cache/                                                              ← omnifs-cache
│   ├── store.rs        Store — object+view façade, per-mount generation fence,
│   │                   CacheError (thiserror; anyhow gone)
│   ├── object.rs       durable canonical store — an object's only home; StoredObject::decode
│   ├── view.rs         derived tier, restart-wiped; Expiry (was cache-side "Freshness")
│   └── blob.rs         key→bytes disk store for payloads that never cross the WIT boundary
│                                                                       ← host blob_cache.rs
├── view.rs             FileAttrsCache, ByteSource, Stability, Freshness, DirentsPayload,
│                       EntryMeta, payload codecs — PUBLIC (frontends read attrs)  ← core/view.rs
├── inflight.rs         InFlight, CoalesceKey — path-ancestor + object-identity coalescing
├── serving.rs          ServingContext — the mount set a request is served against;
│                       registry-backed | single-runtime constructors (tree's test-only
│                       Mounts::Single variant dies); split_mount_path + root-mount claiming
│                       live here                                        ← tree/lib.rs
├── tree/                                                               ← omnifs-tree
│   ├── mod.rs          Tree — the projection authority; PUBLIC; takes a ServingContext
│   ├── resolve.rs      name-oracle: cached-dirent shortcut, control names, subtree handoff
│   ├── list.rs         cache consult → provider → populate; honest exhaustiveness
│   ├── read.rs         read cascade, inline fast path, op_gen-fenced cold render,
│   │                   learned-size promotion; enforces MATERIALIZE_MAX_BYTES → TooLarge
│   ├── handle.rs       RangedHandle, EOF learning, probe_ranged_attrs (contract documented
│   │                   on Tree::list), spawn_live_follow_pump
│   ├── node.rs         Node, NodeBody, NodeId, Entry, EntryOrigin, Synthetic, PaginationControl
│   ├── synthetic.rs    @next/@all + mount-root ignore surface — OWNED here now
│   │                                          (host keeps only the raw paginate primitive in ops/)
│   ├── invalidate.rs   InvalidationReport, drain + mem eviction
│   └── error.rs        TreeError, TreeErrorKind (+ retry_class(), the shared partition)
├── render/             the shared renderer toolkit — PUBLIC, opt-in, owns no kernel state
│   ├── identity.rs     IdentityTable<Id, Body> — DashMap pair, atomic get-or-alloc, the merge
│   │                   rules ("resolution overrides synthetic", "exact size wins");
│   │                   PathKey                                          ← host path_key.rs + both frontends' tables
│   ├── follow.rs       FollowSizeTable — the u64→u64 max-growth map      ← both frontends
│   ├── invalidate.rs   report → stale-ids walker (frontends keep only the terminal action)
│   └── attrs.rs        backing-metadata classification (dir/symlink/file, ro mode bits);
│                       MATERIALIZE_MAX_BYTES                            ← both frontends' twins + nfs-only cap
├── inspect.rs          InspectorSink (::global, ::init_from_env), InspectorRequestScope
│                       (was InspectorFuseScope) — emits omnifs_api::events records
├── sandbox/            publish.rs → PublishRoot (the 7 &Path free fns);  relative_key.rs
├── tree_refs.rs        TreeRefs — u64 → PathBuf registry for clones and extractions
├── object_id.rs        ObjectId — postcard-encoded LogicalId, opaque to the engine
├── log_redaction.rs    LogUrl, WitHeaders display wrappers
├── clock.rs            DYNAMIC_TTL_MILLIS, now_millis
└── test_support.rs     TestOp harness for itest — cfg-gated               ← the 40% of runtime.rs
```

---

## Frontends

### `omnifs-fuse` (linux) — the FUSE protocol codec

```
src/
├── lib.rs              Frontend — engine::render tables; NotifierHandle (a newtype, was a bare
│                       Arc<Mutex<Option<..>>> alias); invalidation drain → kernel notify
├── filesystem.rs       fuser::Filesystem impl — ASYNC-FIRST: each callback moves its owned
│                       Reply into a task on the runtime and returns; bounded in-flight budget
│                       (mirrors NFS RpcSlots); no per-op block_on, no head-of-line blocking
├── inode.rs            NodeEntry — thin wrapper over IdentityTable adding FUSE-only fields
├── listing.rs          DirSnapshot construction from Tree::list; kernel-offset windowing
├── lookup.rs, read.rs  op translations (ranged/full via the shared cap; TooLarge → EFBIG)
├── errno.rs            retry_class → errno (the partition itself is tree-owned now)
├── mount.rs            fuser session lifecycle, unmount
└── trace.rs            per-op timing lines
```

### `omnifs-nfs` — the NFSv4.0 loopback codec

```
src/
├── adapter.rs          Export — thin over IdentityTable/FollowSizeTable; stateid-bound opens
│                       (OpenTable<OpenBody>); eager ranged-attr probing (now a documented
│                       Tree contract, not tribal knowledge); TooLarge → Resource
├── export.rs           Status ↔ nfsstat4, NodeKind, Attr, DirEntry, StateId, OpenTable<B>
│                       (seqid + lease enforcement)
├── delayed.rs          DelayedOps<K,V> — single-flight with wait budget; NFS4ERR_DELAY deferral
├── server.rs           TCP accept, thread-per-RPC, RpcSlots (128 permits), idle reaping
├── mount.rs            macOS mount readiness/teardown — state files and unmount via omnifs-mtab
├── error.rs, trace.rs, lib.rs
└── protocol/           ops.rs (COMPOUND handlers, cookie/cookieverf windows), rpc.rs (ONC RPC),
                        xdr.rs, attrs.rs (fattr4), compound.rs, client.rs (SETCLIENTID),
                        filehandle.rs (generation-prefixed), name.rs, consts.rs
```

---

## Control plane

### `omnifs-daemon` — the server

```
src/
├── app.rs              DaemonArgs, run() — builds Engine + Arc<CredentialService> (spawns the
│                       refresh loop) + frontend; signal handling (Daemon::install_signal_handler)
├── context.rs          DaemonContext — layout via workspace, mount point via
│                       workspace::resolve_mount_point, listener bind, status assembly
├── server.rs           the 13 v2 routes; ApiError mapping; OpenAPI doc; byte-parity test
├── mounts_api.rs       POST/PUT/DELETE /v1/mounts handlers — validate, write through the
│                       advisory-locked Registry, converge one mount; server-side
│                       UpgradePlan::diff vs approved UpgradeDelta (the consent gate)
├── frontends.rs        Frontend enum (Fuse | Nfs), serve/unmount/invalidate dispatch
└── bin/openapi.rs      prints the generated spec for `just openapi`
```

(`proc_mounts.rs` is gone: omnifs-mtab.)

### `omnifs-cli` — the human client

```
src/
├── main.rs, cli.rs     command tree:
│                         omnifs setup | init <provider> [--as] | up | down | status | shell
│                         | logs | doctor | inspect | reset | version | completions
│                         | mounts ls|rm|reauth <name>       (mounts add: REMOVED — init is canonical)
│                         | providers ls|add
│                         | daemon (hidden, feature) | debug mount-tree (hidden)
│                       --json on status, doctor, mounts ls, providers ls, version
├── control/
│   ├── addr.rs         daemon_addr() — THE control-address fact          ← inspector/source.rs
│   └── client.rs       DaemonClient — v2 endpoints, ApiError decoding, bearer token, version gate
├── workspace.rs        Workspace facade — layout + catalog + registry + client, one constructor
├── config.rs           Config (strict TOML), ConfiguredBackend (persisted intent);
│                       resolve_setting() — the ONE flag>env>config>default chain
├── backend.rs          the collapsed identity model: resolve(overrides, config) → LaunchBackend;
│                       DockerTarget (Runtime borrows it; no copied fields)
│                       ← launch_backend.rs's three constructors + session.rs constants
├── launch.rs           Launcher — `up` orchestration; upgrade check via workspace::UpgradePlan;
│                       daemon-mediated writes when one is running
├── runtime_docker.rs   bollard wrapper (pull/create/start/exec/logs)     ← runtime.rs
├── teardown.rs         DaemonTeardown — ONE resolve_running_backend (live status, else launch
│                       record); forced OS path via omnifs-mtab            ← daemon_teardown + host_teardown
├── status.rs           StatusReport — methods, --json; renders CredentialService health
├── doctor.rs           Doctor context struct — probes as methods, incl. a live-daemon probe
├── commands/           thin clap verbs delegating to the modules above
│   └── init/           provider_selection, spec_creation (calls Spec::apply_provider_metadata),
│                       auth_import (consent UX over manifest-declared ambient_sources),
│                       token intake (TokenSource)
├── auth_ux.rs          login flow UX — prompts, browser, device spinner — over CredentialService;
│                       every store write via service.store()              ← auth/{login,mount,readiness,explain}
├── mount_report.rs     UserMountStatus via Spec::pinned_manifest + CredentialStatus
├── mount_config.rs     MountConfig — the crate's most-imported domain type ← session.rs (dissolved)
├── inspector/          the TUI: app, ui, tree (lookup_node_mut now a PathNode method), metrics,
│                       trace_state, run, source (consumes control::addr, doesn't own it)
├── mount_tree.rs       debug wasm route introspection — methods on MountTreeData
├── provider_bundle.rs  embedded provider bundle install (release binaries)
├── error.rs            HintedError + `Try:` hint rendering (thiserror-backed or the dep is dropped)
└── style.rs, token_source.rs
```

---

## Tools and tests

### `omnifs-embed-metadata`

```
src/main.rs             links provider crates natively, converts Provider::METADATA into
                        ProviderManifest, injects omnifs.provider-metadata.v1; hand-maintained
                        provider list guarded by a parity test (cargo-metadata codegen: rejected)
```

### `omnifs-itest`

```
src/lib.rs              RuntimeHarness — drives engine::Runtime via test_support; fixture specs
                        built with Spec::apply_provider_metadata (a production caller at last)
tests/                  per-provider conformance (github, docker, kubernetes, arxiv, dns, oura, …),
                        tree conformance (kernel-free, via ServingContext single-runtime),
                        NEW: auth lifecycle (fake OAuth server: expiry → refresh → rejection →
                        NeedsConsent; reload live-apply), NEW: frontend concurrency (slow provider
                        read must not block a second path), live NFS tests (still serialized)
```

---

## Placement rule (where does new code go?)

| If you are writing | It goes in |
|---|---|
| A type both guest and host must name | `omnifs-core` (if truly guest-needed) or the WIT |
| A capability kind, matcher, or grant shape | `omnifs-caps` |
| Anything serialized over the control wire | `omnifs-api` |
| A new on-disk format, or logic about specs/manifests/credentials/config at rest | `omnifs-workspace` |
| Credential behavior: flows, refresh, health, validation | `omnifs-auth` |
| Anything that runs a provider, executes a callout, touches a cache, or decides projection | `omnifs-engine` (public only if a frontend must see it) |
| Kernel/protocol encoding, handles, wire replies | `omnifs-fuse` / `omnifs-nfs` |
| A REST route or reconcile behavior | `omnifs-daemon` |
| UX: prompts, rendering, command wiring | `omnifs-cli` |
| OS mount-table introspection or unmounting | `omnifs-mtab` |
```
