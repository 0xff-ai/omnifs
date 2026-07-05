# omnifs rework: target architecture and delivery plan

Status: active plan. This revision supersedes all previous bluesky-rewrite text (git history holds it). It is grounded in an eight-track code audit (auth, CLI, control plane, configuration, crate boundaries, tree/frontends, host internals, structural smells) performed 2026-07-02, with file:line evidence throughout, and in a boundary re-derivation that the previous revisions skipped.

Every change is an independently-shippable PR on `main` that keeps the branch green and the live runtime working. There is no fork and no cutover.

## What this fixes

The engine is sound: the WIT callout/effects contract, the sandbox, the async provider runtime, the capability language, the cache coherence model, and the object SDK are well designed and tested. The debt:

- **Auth is a reactive patchwork.** `omnifs-auth` is a stateless OAuth protocol client; the real state machine (freshness window, singleflight refresh, 401/403 retry, token cache) lives in `omnifs-host/src/auth.rs`, readiness reporting lives in `omnifs-cli/src/auth/readiness.rs`, and nothing refreshes ahead of need. Expiry logic exists three times with three skew constants. `omnifs init --reauth` cannot reach a running daemon. Revocation is implemented with zero callers.
- **The API is starved.** Seven routes, two mutating, neither taking a body. No mount CRUD, no credential status, no structured errors, no backend identity, no auth on the control port. Every real operation side-channels through disk plus a blind reconcile, and the re-consent gate on capability-widening upgrades is bypassable because reconcile never consults `UpgradePlan::diff`.
- **The CLI has inverted and quintuplicated facts.** `daemon_addr()` is owned by the inspector debug module. "Which backend" has five representations. Credential-key derivation is duplicated between CLI and host with a hand-copied sentinel. `reset` bypasses `mount::Registry` with raw `fs::remove_file`.
- **Configuration has shadow authorities.** The documented inheritance function `Spec::apply_provider_metadata` is dead in production while three reimplementations run in the CLI. Three atomic-write mechanisms exist. The `auth_wire` manifest tree is serde-permissive inside a strict `ProviderManifest`.
- **The wasm-safe leaf is not wasm-safe.** `omnifs-core` compiles a 973-line host cache codec, credential addressing, spec-name validation, and `utoipa`-deriving catalog types into every provider guest.
- **The frontends re-decide what tree decided.** FUSE and NFS each hand-roll identity tables, follow-size maps, invalidation walks, attr classification, and the `TreeErrorKind` partition. NFS has a 64 MiB materialize cap FUSE lacks. FUSE runs one worker thread with per-op `block_on`, so one cold provider fetch head-of-line blocks the mount.

And beneath all six: **the crate topology draws walls where the domain has none.** Twenty-one crates serve one host-side binary plus one guest target. Almost every audited violation is code tunneling through one of those non-walls: `omnifs-tree` forces `omnifs-host` to make `wit_protocol`, `pagination`, and `Runtime` internals `pub` so a sibling crate can reach them; `blob_cache` grew inside host because `omnifs-cache` was another crate away; the auth state machine grew inside host for the same reason; spec-default inheritance was reimplemented in the CLI because the canonical function lived across a boundary; four types in three crates encode "which backend". A taxonomy too fine for the domain is also why many agents over time produced patchwork: every addition faced a 21-way placement choice and chose differently. The fix is not more crates with cleaner edges. It is fewer crates whose walls are the real ones.

## Merge-blocking bars

- One concept, one name, one owner.
- Each crate states its invariant in one sentence; content that serves another crate's sentence moves.
- No abstraction without two real pressures.
- Producer co-edited with consumers in one pass; `main` green and the live runtime working at every merge.
- The provider contract may change, but the WIT, the SDK macro, the manifest, and all in-repo providers move together in one PR; no provider is ever broken across a PR boundary.

---

# Part one: target architecture

## 1. The real walls, and the crate tree they imply

Compilation boundaries deserve compiler enforcement only where the system has an actual boundary. omnifs has four, plus one economic sub-wall:

- **W1, the guest wall.** Code compiled into provider WASM must be minimal and wasm-clean. This wall is enforced by the SDK's dependency list and nothing else.
- **W2, the trust wall.** The WIT contract. Everything host-side of it is one trust domain, one binary, one Unix user. There is no trust boundary between host-side crates; splits there are organizational only.
- **W3, the protocol walls.** FUSE and NFS are independent protocol codecs (fuser vs hand-rolled XDR/RPC/COMPOUND), platform-gated, over one projection interface.
- **W4, the control wire.** The versioned REST contract between CLI and daemon. A real wire boundary even inside one binary.
- **W5, the state sub-wall.** The CLI must manage everything under `OMNIFS_HOME` (specs, credentials, provider store, config) without linking wasmtime. State management sits below the runtime.

Everything else, including today's host/tree/cache/creds/auth/mount/provider partitions, is module structure, not crate structure. The consolidation is also the encapsulation fix, not a concession: today `wit_protocol.rs` is `pub`, `pagination` constants are `pub`, `Materializer` projection methods are `pub`, and cache internals are `pub` *because* tree and the frontends are separate crates that need them. Inside one engine crate all of it goes `pub(crate)`, and the frontends can only see the curated projection surface. "Frontends translate, tree decides" stops being a convention and becomes visibility.

### Target tree (16 crates + providers)

The full file-and-symbol layout, with provenance for every move, lives in the companion [target source tree](bluesky-tree.md); the table below is the invariant-level summary.

| Crate | The one sentence | Absorbs / sheds |
|---|---|---|
| `omnifs-core` | What a provider guest may know: `Path`, `Segment`, `ContentType`. | Sheds `view.rs` (to engine), `auth.rs` (to workspace), `mount.rs` (to workspace), `provider.rs` (to workspace). ~580 lines remain, matching its actual guest imports (verified: every `omnifs_core::` use in `omnifs-sdk` and `providers/*` is `Path`, `Segment`, or `ContentType`). |
| `omnifs-wit` | The provider contract. | Unchanged. |
| `omnifs-caps` | The capability language: `Need`/`Grant<T>`/`Grants`/`Allowlist` model and matching. | Unchanged (its own doc already states enforcement lives elsewhere, `caps/lib.rs:15-16`). |
| `omnifs-api` | The control-plane wire contract: REST DTOs, error codes, API version, and the inspector event schema. | Absorbs `omnifs-inspector` (804 lines): REST DTOs and `/v1/events` records are the same contract with the same two consumers (daemon emits, CLI consumes). |
| `omnifs-mtab` | Reading and unwinding OS mount state: the `/proc/mounts` parser, `NfsMountState` files, per-platform unmount command builders. | New; replaces the byte-identical parser copies (`omnifs-daemon/src/proc_mounts.rs:13-53` = `omnifs-nfs/src/mount.rs:427-465`), the forked unmount builders (`nfs/mount.rs:162-225` vs `cli/host_teardown.rs:268-357`), and the hand-synced version constant (`nfs/mount.rs:52` vs `cli/host_teardown.rs:25`). |
| `omnifs-sdk-macros` | Provider authoring macros. | Unchanged crate; internals unified in §7. |
| `omnifs-sdk` | The provider authoring surface. | Unchanged crate; internals in §7. Depends on core, wit, macros and nothing else: the W1 gate. |
| `omnifs-workspace` | Every byte under `OMNIFS_HOME`: mount specs and `Registry`, provider store/catalog/manifests, the credential store, CLI config, launch record, workspace layout. | Merges `omnifs-mount` + `omnifs-provider` + `omnifs-creds` + `omnifs-home`. One owner for all on-disk formats means one atomic-write mechanism, one strictness policy, and `Spec::apply_provider_metadata` living beside the creation flow that must call it. |
| `omnifs-auth` | Credential lifecycle: OAuth flows plus the proactive `CredentialService` (§2). | Depends on workspace (store, scheme types) and core. Sheds nothing; gains the state machine currently in host. The previously planned `omnifs-auth-model` leaf dissolves: it existed only to break the auth→mount/provider inversion, and the workspace merge breaks it structurally. |
| `omnifs-engine` | The trusted runtime: sole wasmtime linker, callout executor, capability enforcer, cache writer, effect applier, and projection authority. | Merges `omnifs-host` + `omnifs-tree` + `omnifs-cache` + core's `view.rs`. Internal modules: `runtime`, `instance`, `callouts`, `auth_inject`, `effects`, `cache` (object/view/blob), `view`, `tree`, `render` (§5), `inspect`. Public surface: engine construction, `ServingContext`/`Tree`, the render toolkit, health, the event stream. Everything else `pub(crate)`. The previously planned `omnifs-view` crate dissolves: view types become `engine::view`, public to frontends, structurally unreachable from the SDK. |
| `omnifs-fuse` | The FUSE protocol codec. | Depends on engine, core, mtab. |
| `omnifs-nfs` | The NFSv4.0 loopback protocol codec. | Depends on engine, core, mtab; its `omnifs-cache` production dependency (used only in tests, `nfs/Cargo.toml:13`) dies in the merge. |
| `omnifs-daemon` | The server: hosts the engine and the `CredentialService`, serves the REST contract, reconciles. | Depends on api, engine, auth, workspace, frontends, mtab. |
| `omnifs-cli` | The human client: UX over the REST API, plus direct workspace operations when no daemon runs. | Depends on api, auth, workspace, caps, core, mtab (+ daemon/nfs features). |
| `omnifs-embed-metadata` | Build-time manifest harvester. | Unchanged. |
| `omnifs-itest` | Host-driven conformance. | Unchanged role; retargets to engine + workspace. |

Layering, bottom-up: `core, wit, caps, api, mtab` → `sdk-macros, sdk` (guest branch) and `workspace` → `auth` → `engine` → `fuse, nfs` → `daemon` → `cli`.

### What the consolidation buys, concretely

- **The tunneling stops being possible.** `wit_protocol`, `pagination`, `Materializer`'s projection methods, and cache schema go `pub(crate)` in engine (today `pub` solely for tree/frontends: `omnifs-host/src/wit_protocol.rs`, `namespace.rs:52-176` returning raw wit types, `omnifs-tree` importing `omnifs_wit` in five files). Frontends physically lose access to `Runtime`, `Namespace`, and wit types.
- **One home per fact.** The backend-identity quintuplication, the inheritance triplication, the three atomic-write mechanisms, the credential-key duplication: each becomes an intra-crate unification instead of a cross-crate negotiation.
- **Placement stops being a 21-way choice.** A contributor or agent adding code picks between six meaningful homes (vocabulary, sdk, workspace, auth, engine, surface). The one-sentence table above is the placement rule.
- **The clean crates stay untouched.** caps, wit, sdk's outer boundary, and the frontends' protocol ownership were audited clean; the consolidation does not churn them.

Costs, honestly: engine is a ~20k-line crate (comparable to today's cli at 14.3k and sdk at 12.5k; wasmtime dominates its compile either way, and today's tree→host→cache chain already compiles serially). The tree/host dependency direction stops being compiler-checked and becomes module visibility plus the kernel-free tree conformance tests, in exchange for the frontend-facing surface becoming compiler-checked, which is the direction the product contract actually cares about.

### Considered and rejected restructures

Recorded so the search is visible and not re-run:

- **Folding the frontends into engine.** Rejected: W3 is real; NFS is 8.9k lines of independent protocol machinery, FUSE is platform-gated, and both consume only the projection surface once §5 lands.
- **Merging daemon and cli into one crate.** Rejected: they are the two ends of W4; keeping them apart with `omnifs-api` between them is what keeps the CLI honest about going through the wire.
- **Splitting CLI and daemon into two binaries.** Rejected: the single-binary shape is a settled product decision (`docs/architecture/00-overview.md:80`) and nothing in the audit indicts it.
- **Replacing the hand-rolled NFSv4 server with a library.** Rejected for now: no maintained Rust NFSv4.0 server crate is known (nfsserve is v3), and v4.0 stateids/leases are the reason macOS loopback works. Re-evaluate only if the ecosystem changes.
- **A database or single-file store for specs/config.** Rejected: one file per mount under a human-editable workspace is product surface, not an implementation detail.
- **Folding `omnifs-home` into core** (previous revision). Rejected then and still: workspace resolution is host environment behavior; it lands in `omnifs-workspace` instead, where the layout belongs with the formats it roots.
- **Keeping the fine-grained topology and only fixing edges** (the previous revision of this plan). Rejected by the evidence above: the edges were symptoms.

## 2. Auth: a proactive credential subsystem

### The diagnosis, compressed

Today `omnifs-auth` executes one flow at a time and owns nothing (`crates/omnifs-auth/src/client.rs`). The runtime brain is `omnifs-host/src/auth.rs`: per-mount strategies built once at instance construction, an `ArcSwapOption<CredentialEntry>` cache, a 60-second freshness window checked synchronously inside the request path (`OAUTH_REFRESH_WINDOW`, `auth.rs:20`; `refresh_if_needed`, `auth.rs:382-412`), refresh on 401 or 403+`invalid_token` (`http.rs:87-101`, `auth.rs:497-508`), and credential deletion as a side effect of `invalid_grant` (`auth.rs:445-451`). The CLI owns login UX, ambient-credential import, and readiness (`omnifs-cli/src/auth/*`), all by reading `credentials.json` directly. There is no background refresh, no mount-start validation, no daemon-visible credential state (`omnifs-api/src/lib.rs:30-32` documents the absence), no live-apply for re-auth (`commands/init/mod.rs:283-285`), and no revocation caller (`client.rs:196-210`). Expiry is computed three ways: mint-time skew (`omnifs-auth/src/client.rs:487-490`), `CredentialEntry::is_expired_at` (`omnifs-creds/src/lib.rs:128-130`), and `oauth_entry_is_fresh` (`omnifs-host/src/auth.rs:550-560`).

### Target: `CredentialService` in omnifs-auth

`omnifs-auth` becomes the single owner of credential lifecycle. It keeps the flow client (`OAuthClient`) and gains a service:

```rust
// omnifs-auth
pub struct CredentialService { /* store handle, OAuthClient, per-credential state table */ }

pub enum CredentialHealth {
    Ready,                        // valid, not near expiry
    ExpiringSoon,                 // inside the refresh window; refresh scheduled
    Expired,                      // past expiry, refresh impossible or not yet run
    RefreshFailed { attempts: u32 },
    NeedsConsent,                 // invalid_grant or revoked upstream: user must re-login
    Missing,                      // spec references a credential the store lacks
    StaticUnvalidated,            // static token, no probe since stored_at
}

impl CredentialService {
    /// The one injection entry point. Returns current header material,
    /// refreshing synchronously only if the proactive loop has not already.
    /// Fails closed: Missing or NeedsConsent yields a typed error, never
    /// silent unauthenticated traffic.
    pub async fn authorization(&self, id: &CredentialId) -> Result<HeaderMaterial, AuthUnavailable>;

    /// Backstop for upstream rejection the loop cannot see (401, 403 +
    /// WWW-Authenticate invalid_token). Single-flight refresh, then a health
    /// transition; never deletes the stored secret.
    pub async fn report_rejected(&self, id: &CredentialId, evidence: RejectionEvidence) -> RefreshOutcome;

    /// Health snapshot for the daemon status endpoint and the CLI.
    pub fn health(&self) -> Vec<CredentialStatus>;

    /// Control-plane push: re-read one credential from the store and swap it
    /// into live state. This is what makes re-auth apply without a restart.
    pub async fn reload(&self, id: &CredentialId);

    /// Store a login/refresh result and update state. All CLI login flows
    /// funnel through this instead of writing the file store directly.
    pub fn store(&self, id: &CredentialId, entry: CredentialEntry) -> Result<(), StoreError>;

    /// Upstream revocation + local delete, wired to `mounts rm`/`reset`.
    pub async fn revoke_and_delete(&self, id: &CredentialId) -> RevokeOutcome;

    /// The proactive loop: one sweep task waking at the earliest
    /// (expiry - refresh window) across OAuth credentials, refreshing with
    /// jitter and per-id single-flight; optionally re-runs a provider's
    /// TokenValidation probe on a slow cadence for static tokens.
    pub fn spawn_refresh_loop(self: &Arc<Self>) -> tokio::task::JoinHandle<()>;
}
```

Ownership consequences, each a concrete move:

- **One expiry function.** `is_expired_at` plus a single `REFRESH_WINDOW` constant in `omnifs-auth`; the mint-time skew and host's independent `oauth_entry_is_valid`/`oauth_entry_is_fresh` are deleted.
- **One credential-key derivation.** `CredentialId::for_mount(provider: &ProviderName, auth: &spec::Auth) -> CredentialId`, replacing the parallel derivations in `omnifs-cli/src/credential_target.rs:95-113` and `omnifs-host/src/auth.rs:294-299,343-348` and deleting host's hand-copied `DEFAULT_ACCOUNT` (`auth.rs:19`). A drift here is a silent auth outage; today nothing pins the two sides together.
- **Engine keeps injection only.** `engine::auth_inject` keeps what only the trust boundary can own: per-mount domain matching, header composition, and the guarantee that tokens are injected only for allowed destinations. Freshness, refresh, retry classification, and store access move behind `authorization`/`report_rejected`. The `HttpStack::send` retry-once loop stays but delegates the decision.
- **`invalid_grant` stops deleting secrets.** The service transitions to `NeedsConsent`, keeps the entry for diagnostics, and surfaces the state through health (today: mid-request deletion, `omnifs-host/src/auth.rs:445-451`).
- **Mount start validates.** Engine asks the service for the mount's credential health at build and marks a degraded mount immediately instead of silently loading an expired credential (`runtime.rs:339-355` today).
- **The CLI keeps UX only.** Prompts, browser opening, device-code spinners, ambient-credential consent stay; every store write goes through `CredentialService::store`; readiness is service-computed and CLI-rendered. Ambient-credential detection (today a hardcoded `match` on `"github"`/`"linear"`, `commands/init/detect.rs:20-49`) becomes manifest-declared: an optional `ambient_sources` list on the static-token scheme.
- **The GitHub device-flow shim becomes a declared protocol variant.** `rewrite_pending_to_error_status` (`omnifs-auth/src/client.rs:373-430`) names a vendor inside the crate whose architecture doc forbids vendor knowledge (`docs/architecture/40-auth-boundary.md:10-14`). Fix: `device_poll_compat: Rfc8628 | ErrorInOkBody` on the device-flow config, declared by the provider's scheme.
- **Re-auth applies live.** `omnifs mounts reauth <name>` runs the flow, calls `store`, and hits `POST /v1/credentials/{id}/reload` (§3) when a daemon runs. No restart.
- **Where the service runs.** The daemon constructs one `Arc<CredentialService>` and hands it to the engine. The CLI constructs its own for daemon-down operation; both share the advisory-locked file store, and the reload endpoint keeps a running daemon coherent after a CLI write.

Auth invariants preserved: credentials never on the wire (health carries state, expiry timestamps, scopes; never token material; credential types never derive `ToSchema`). Store stays 0600/0700, atomic, advisory-locked. Providers never see tokens; injection happens only engine-side after the callout crosses the WASM boundary.

## 3. Control plane: API v2 and mount lifecycle

### The diagnosis, compressed

The whole surface is `GET ready`, `GET status`, `GET mounts`, `GET mounts/{name}`, `POST reconcile`, `POST shutdown`, `GET events` (`omnifs-daemon/src/server.rs:260-269`). Mount create/remove/upgrade are disk writes plus a blind reconcile (`commands/init/mod.rs:156`, `commands/mounts.rs:107-116`, `upgrade.rs:126-154`). Failures are all-200 with `reason: String` (`omnifs-api/src/lib.rs:202-206`). `MountInfo.provider_id` holds a name slug by admission (`lib.rs:195`). Daemon-side reconcile never consults `UpgradePlan::diff`, so the CLI's re-consent gate on capability/auth widening is bypassable by anything that writes a spec and calls `/v1/reconcile` (`omnifs-host/src/registry.rs:684-695` vs `omnifs-mount/src/upgrade.rs:33-90`). The control port has no authentication. Backend identity (container name/image, native PID) lives only in the CLI's `launch.json`, whose corruption fallback guesses (`launch_record.rs:174-203`).

### Target surface (API_MAJOR = 2)

| Method | Path | Body / response | Notes |
|---|---|---|---|
| GET | `/v1/ready` | `ReadyInfo` | unchanged |
| GET | `/v1/status` | `DaemonStatus` v2 | adds backend identity (container name/image or native pid), per-mount credential health summary, last-reconcile outcome per mount |
| GET | `/v1/mounts` | `Vec<MountSummary>` | `provider_name` plus pinned `provider_id` content hash, health, auth state |
| POST | `/v1/mounts` | `Spec` in, `MountReport` out | daemon validates, writes through `Registry`, converges that one mount |
| GET | `/v1/mounts/{name}` | `MountDetail` | grants, auth scheme + readiness (no secrets), pinned artifact hash, artifact-present, reconcile history |
| PUT | `/v1/mounts/{name}` | `Spec` + `approved: UpgradeDelta` | recomputes `UpgradePlan::diff` server-side; 409 with the actual delta if it exceeds the approval |
| DELETE | `/v1/mounts/{name}` | `MountReport` | atomic spec removal + converge, replacing the two-step delete-then-maybe-reconcile |
| GET | `/v1/providers` | `Vec<ProviderSummary>` | surfaces `Catalog::installable()`/`latest_by_name()` |
| GET | `/v1/credentials` | `Vec<CredentialStatus>` | health only, never secrets |
| POST | `/v1/credentials/{id}/reload` | `CredentialStatus` | the re-auth live-apply hook (§2) |
| POST | `/v1/reconcile` | optional `{ mounts: [...] }`, `ReconcileReport` | gains single-mount scope; 409 + `Retry-After` when a pass is running, instead of silently queueing on `reconcile_lock` |
| POST | `/v1/shutdown` | `StopReport` | unchanged |
| GET | `/v1/events` | NDJSON | unchanged |

Cross-cutting:

- **Structured errors everywhere.** One `ApiError { code: ErrorCode, message, detail? }` body on every non-2xx; `MountFailure.reason` gains a typed `kind`. The CLI stops parsing prose.
- **Spec authorship follows the daemon.** When a daemon runs, the CLI calls the mounts endpoints and the daemon writes through `Registry`; when none runs, the CLI writes through the same `Registry` directly. `Registry` stays the sole owner in both processes and gains the same advisory file lock the credential store already uses, closing the cross-process write race the single-author assumption papers over. `docs/contracts/50-control-plane.md` updates in the same change.
- **The re-consent gate becomes daemon-enforced.** Reconcile computes `UpgradePlan::diff` against the running mount's pinned manifest and refuses to hot-swap on a capability- or auth-widening delta without an approval covering it. Interactive consent stays in the CLI; enforcement moves to where the swap happens. Local disk tampering stays out of scope (the workspace is inside the local trust boundary), but the API path can no longer skip consent.
- **`UpgradePlan::diff` binds the full surface it gates.** Today it compares only the default scheme key (`upgrade.rs:41-45`). It must also diff OAuth endpoints, scopes, inject domains and header, flow kind, config field types, and capability grants. The approval delta in `PUT /v1/mounts/{name}` is checked against this.
- **Control-port authentication (gated decision).** Loopback binding is not authentication; the Docker port-forward exposes the port to every local process. Proposal: a bearer token generated at daemon start, written to `config_dir/control-token` (0600), sent by the CLI on every request. Surface and confirm before landing.
- **Wire warts fixed at the major bump.** `MountInfo.provider_id` renamed to `provider_name`; a real `provider_id` (content hash) added. `FrontendInfo.fs_type` becomes an enum.
- **Deferred, explicitly.** Metrics aggregation endpoints (error rate, cache-hit ratio, p95) stay client-side in the inspector TUI until the engine's cache grows counters; no empty scaffolding.

## 4. Configuration: one owner per datum

The workspace merge (§1) makes most of this structural rather than aspirational; the remaining items are behavior:

- **One inheritance function.** `Spec::apply_provider_metadata` (`omnifs-mount/src/mounts/mod.rs:90-120`) has zero production callers while `MountSpecCreator::create` (`cli/commands/init/spec_creation.rs:22-42`), `AuthSelection::from_provider_default` (`cli/auth/mount.rs:27-39`), and `apply_additive_upgrade` (`cli/upgrade.rs:126-154`) each reimplement the fold. It becomes the sole implementation, called by init, upgrade, and itest; the shadows die. It subsumes config defaults, auth defaults, and capability inheritance.
- **Strict parsing.** Top-level `Spec` gains `deny_unknown_fields` (gated decision: this document is the surfacing; confirm at that slice's review). The `auth_wire` scheme structs gain it (today only `TokenValidation` has it). Provider-store `Index`/`IndexEntry` gain it.
- **One atomic-write mechanism.** Workspace standardizes on the `atomic_write_file` crate (already used by creds); the hand-rolled temp+rename in `mounts/mod.rs:364` and the bare `fs::write` in `ConfigFile::save` (`cli/config.rs:99-108`) move onto it. Engine's `sandbox/publish` directory-rename toolkit stays; publishing extracted trees is a different job.
- **One precedence helper.** The documented chain (flag > env > config > default, `cli/config.rs:1-4`) is hand-rewritten per datum (`launch_backend.rs:122-145`, `inspector/source.rs:15-18`). One `resolve_setting` helper, used by every resolver.
- **Single owners for duplicated values.** `omnifs_api::default_listen_addr()` replaces four hand-built `SocketAddr`s (`daemon/app.rs:70-72`, `cli/launch.rs:217`, `cli/client.rs:17`, `cli/inspector/source.rs:17`). `resolve_mount_point` exposed from workspace so setup's preview (`cli/commands/setup/mod.rs:93-99`) cannot drift from the daemon's answer (`daemon/context.rs:223-231`) under `OMNIFS_MOUNT_POINT`. The dead `WorkspaceLayout::wasm_cache_dir` and host's live duplicate (`omnifs-host/src/runtime.rs:96-98`) collapse to one. The daemon log-level literals (`"warn"` in `cli/main.rs:55`, `"info"` in `launch_backend.rs:277`) become one constant with the foreground/spawned distinction explicit.
- **Inject domains must be covered by capability needs.** The same hostname is hand-maintained in `capabilities(domain(...))` and `.inject([...])` with no cross-check (`providers/github/src/lib.rs:44-46,72,85-98`). Manifest validation fails a scheme whose inject domain no declared domain need covers.
- **Guest path literals get a CI tripwire.** `/root/.omnifs` (5 sites) and `/omnifs` (3 sites) span Rust, Dockerfile, shell, and TypeScript; a CI grep asserts agreement.
- **Dockerfile duplication.** `scripts/ci/Dockerfile.runtime` is a hand-copied twin of `Dockerfile`'s runtime stage with no sync marker; extract a shared stage or generate one from the other, and cross-reference both in `docs/contracts/60-build-validation.md`.
- **`dev.ts` stops hand-writing owned formats.** It writes specs and credentials with its own non-atomic, non-locked writers (`scripts/dev.ts:294-299,513-531`) and hand-maintains seven `providers/*/dev/mount.json` fixtures that drift silently. Route dev-home rendering through the CLI (`omnifs init --no-input`, or a hidden render command) so `Registry` and the credential store stay the only writers and fixtures regenerate from manifests.

## 5. Projection and frontends

### What stays settled

The tree code is genuinely protocol-clean: no wit types in public signatures, no `block_on`, no FUSE/NFS concepts in code. It correctly consumes coalescing and fencing through `Namespace` and never constructs a raw cache store. All of that survives the merge into `engine::tree` unchanged, with the kernel-free conformance harness still driving it.

### The seam fixes

- **All five namespace outcomes materialize inside engine.** `lookup_child` already returns a domain `LookupOutcome`; `list_children`/`read_file`/`open_file`/`read_chunk` return raw wit types (`omnifs-host/src/namespace.rs:52,93,160,176`). Extend the pattern to all five; `wit_protocol` goes `pub(crate)`; the five copy-pasted dispatch-and-match blocks in `namespace.rs` collapse into one generic `run_op_expect`.
- **`engine::render`: the shared renderer toolkit.** Opt-in, outside `Tree`'s decision API (the "tree owns no kernel state" rule stands):
  - `IdentityTable<Id, Body>`: the DashMap pair, atomic get-or-allocate, and the merge rules ("provider resolution overrides synthetic marker", "learned/exact size wins") factored from `omnifs-fuse/src/inode.rs:241-289` and `omnifs-nfs/src/adapter.rs:334-400`. FUSE's and NFS's `NodeEntry` types become thin wrappers adding only protocol fields (`scope`, `parent`, `size_exact`). `path_key.rs` (zero host-internal users) folds in.
  - `FollowSizeTable`: the identical u64→u64 max-growth map both frontends hand-roll around the shared pump (`fuse/lib.rs:92` + `read.rs:314-325`; `nfs/adapter.rs:126-146`).
  - An `InvalidationReport` consumer that walks an identity table and returns stale ids; each frontend keeps only its terminal action (kernel notify vs stateid teardown), factored from `fuse/lib.rs:194-221` and `nfs/adapter.rs:282-328`.
  - `TreeErrorKind::retry_class()` owning the retryable/gone/terminal partition (today forked: `fuse/errno.rs:17-28`, `nfs/adapter.rs:568-580`); frontends map class to errno/nfsstat only.
  - Shared backing-metadata classification (dir/symlink/file, read-only mode bits), factored from the twin `attr_from_metadata`s (`fuse/inode.rs:337-366`, `nfs/adapter.rs:434-468`).
- **One materialize cap, tree-enforced.** NFS caps whole-file materialization at 64 MiB (`nfs/adapter.rs:985-1015`); FUSE has no cap and buffers arbitrarily large files (`fuse/read.rs:190-209`). The budget moves into `Tree::read`/`open` with a `TreeErrorKind::TooLarge` outcome (the variant exists); FUSE maps `EFBIG`, NFS maps `Resource`.
- **FUSE goes async-first.** fuser's session runs one worker thread by default and every callback does one `block_on`, so a cold provider op head-of-line blocks the mount; the recent responsiveness work (`NFS4ERR_DELAY`, thread-per-RPC) landed for NFS only. Each FUSE callback moves its owned reply object into a task on the runtime and returns immediately, with a bounded in-flight budget mirroring NFS's `RpcSlots`. Gate: an itest holding one slow provider read while a second path serves within budget.
- **Pagination's projection face moves to `engine::tree`.** The `@next`/`@all` names, ignore-file bytes, and synthetic dirent construction (`omnifs-host/src/pagination.rs:38-108`) are directory-listing UX that tree already re-exports as "the synthetic surface" (`omnifs-tree/src/synthetic.rs:12-14`); post-merge this is a module move, with the raw fetch-next-page primitive and cache-backed accumulation staying in `engine::runtime`.
- **Tree contract documentation.** `Tree::list`'s ranged-placeholder entries may need a follow-up `probe_ranged_attrs`/`open` for frontends that cannot stat lazily; today that is NFS tribal knowledge (`nfs/adapter.rs:531-557`). Document it on `Tree::list`.
- **Dead code.** `Tree::cached_file_attrs`/`publish_file_attrs` (`tree/read.rs:116-133`) have zero callers; delete or wire.
- **`Mounts::Single` dies with `ServingContext`.** `Tree` embeds a test-only enum variant (`tree/lib.rs:61-67`). `ServingContext` (`engine::serving`: the mount set a request is served against, resolving names to live runtimes; no policy, no scope claims) gets the two constructors (registry-backed, single-runtime); `Tree` takes it; mount-path splitting and root-mount claiming (`split_mount_path`, `lib.rs:137-170`) move onto it. The `Worldview` name still waits for enforced scope.

## 6. Engine hygiene

- **Delete `manifest::Artifact`.** One call site wrapping two lines of `ProviderWasm` (`omnifs-host/src/manifest.rs:11-22`, used only at `runtime.rs:318-320`), colliding with the real `omnifs_provider::Artifact`. Inline it; one `Artifact` remains. The trust-critical pinned-manifest read in `Runtime::build` keeps reading the manifest from the pinned artifact bytes, without the wrapper.
- **Split the test harness out of `runtime.rs`.** `TestOp`/`PendingTestCallout`/`__test_support` interleave ~40% of the file with production lifecycle; move to a gated `test_support` module.
- **Two extension traits replace the converter sprawl.** A `FromWit`/`ToWit` pair collapses the twelve orphan-blocked converters in `wit_protocol.rs`; a `CalloutResultExt` collapses the eight `callout_*` constructors only if the churn pays; they are otherwise fine as named error constructors (~40 call sites; the old plan's "delete them" was wrong).
- **Registry duplication.** The near-identical claim-root-mount blocks in `publish_new_mount` and `replace_mount` (`registry.rs:136-148,179-196`) factor into one helper. `mount_fingerprint` moves to BLAKE3 over the spec JSON (hygiene; in-memory only).
- **Naming.** `InspectorFuseScope` becomes frontend-neutral. `tools/mod.rs`'s "Wasm tools" doc comment is corrected (the module is host-native Rust). Post-merge module names settle the old collisions: `ProviderRegistry` becomes `engine::runtime::MountRuntimes` (vs `workspace::mounts::Registry`), host's `Materializer` becomes `engine::effects::EffectApplier` (vs `workspace::materialize`), cache's `Freshness` fence renames to `Expiry` (vs view's content `Freshness`); the two `StoreError`s, two `ResolveError`s, two `AuthError`s, and five bare `Error` enums gain domain-qualified names as their homes merge.

## 7. SDK and macros

Unchanged from the audit-backed plan; the guest wall is the one boundary that was already right:

- **`Collection<C>` is the only listing surface**; `DirProjection` dies and Linear migrates to `Collection<Issue>`.
- **Two-stage registration** replaces the `Rc<RefCell<Option<Rc<...>>>>` late-binding cell; the string-matched parallel `collections`/`collection_handlers` lists merge into one declaration.
- **`router/object.rs` splits.** 2049 lines, the largest file in the workspace, no inline tests. Seams: registration, dispatch, serve pipeline. `ServeCtx` (defined at `object.rs:1604` with no impl) gains its four serve functions as methods.
- **Typed object kinds.** The seal-time collection/object match compares raw `&'static str` (`register.rs:376`) while `ObjectKind` exists; store the newtype throughout.
- **Macros converge on one shape.** Three macros use Args-struct plus free codegen functions; `endpoint_macro.rs` inlines everything in one 174-line function. Unify on Args-struct with `expand()`; the god function falls out.
- **`helpers.rs` dissolves** (`err()` duplicates the `From<ProviderError>` impl; `pretty_json` belongs in `repr.rs`). The pattern module's `Result<T, String>` shadow error channel (`router/pattern.rs:108,206,225,424`) gets a real error type.
- **Conformance and tracers gate everything**: the kernel-free harness plus GitHub/Linear/Docker live smoke on every authoring-surface change.
- **The `Need` → `AccessNeed`/`ResourceLimit` split** remains the one contract-touching slice (WIT `capability-need`, macro, manifest, all providers, one PR), per `docs/future/capability-limits-split.shapediff.md`.

## 8. CLI shape

The experience this structure serves (golden paths, output contract, exit codes, doctor-as-triage, agent path) is specified in the companion [CLI experience masterplan](cli-experience.md); this section is the structural half.

- **`daemon_addr` moves out of the inspector.** It becomes the control-client module's fact; the inspector imports it. Today the debug TUI owns the address every production command resolves (`inspector/source.rs:15-18`, consumed by `client.rs:78` and `launch.rs:224-226`).
- **One backend-identity model.** `ConfiguredBackend` (persisted intent) and the widened `DaemonBackend` (live fact carrying container name/image or native PID, §3) are the two legitimate representations. `LaunchBackend`'s three constructors collapse into one `resolve(overrides, config)`; `launch.json` becomes a cache of daemon-reported identity for the daemon-down case, and its guess-the-container fallback (`launch_record.rs:174-203`) dies because `DaemonStatus` now answers while alive. `Runtime`'s copied name/image fields take a `DockerTarget`. `DaemonTeardown`'s two probe paths merge into one `resolve_running_backend`.
- **`Registry` bypasses close.** `reset` routes spec deletion through `Registry::remove` (today raw `fs::remove_file`, `commands/reset.rs:67`); `mount_report::artifact_present` calls `pinned_manifest` instead of reimplementing it (`mount_report.rs:62-70`).
- **Command surface.** `init` is the canonical creation flow; `mounts add` (a literal forward, `commands/mounts.rs:44`) is removed. Re-auth becomes `omnifs mounts reauth <name>`, ending the `--reauth` mode that repurposes the `provider` positional as a mount name (`commands/init/mod.rs:234`). `--json` lands on `status`, `doctor`, `mounts ls`, `providers ls`, `version`. `doctor` gains a live-daemon probe section (today it never talks to the daemon). Thin-command discipline: `doctor`, `shell`, and `setup` logic moves behind top-level modules as `up`/`down`/`status` already do.
- **`session.rs` dissolves** (launch constants to `launch_backend`, `MountConfig` to its own module, `env_string` into the §4 resolution helper). Readiness rendering delegates to `CredentialService::health()`; the CLI's local expiry arithmetic dies. `thiserror` (declared, zero uses) is used or dropped.

## 9. Method and helper hygiene

Policy: a module-level function whose first parameter is (a reference to) a locally-defined type becomes a method or associated function; converters onto foreign types get a local extension trait; functions that build a local type become constructors. The audit found the violations concentrated; each cluster lands as a mechanical per-crate slice:

| Home (post-merge) | Cluster | Fix |
|---|---|---|
| engine | `wit_protocol.rs` (12 fns), `callouts.rs` (8 fns) | `FromWit`/`ToWit` + optional `CalloutResultExt` (§6) |
| engine | `tools/archive.rs::extract`, `inspector.rs::{global, init_global_from_env, subtree_tree_ref}`, `namespace.rs::enoent`, `capability.rs::config_str` family, `sandbox/publish.rs` (7 fns over `&Path`) | methods on `ArchiveFormat`, `InspectorSink`, `Error::not_found`, the capability checker; a `PublishRoot` struct |
| engine::tree | `list.rs`/`read.rs`/`resolve.rs` free fns threading `&Runtime`/`&Node` | `Node` methods where the subject is local; the rest become tree-ops methods once `ServingContext` lands |
| workspace | `pinned_manifest`, `materialize()`, six `upgrade.rs` diff helpers, `records.rs::encode_*`, `resolve_manifest`, `read_provider_metadata_file` | `Spec::pinned_manifest`, `Spec::materialize`, private associated fns on the diff type, `rec.encode()`, `ResolvedManifest::resolve`, associated fn on `ProviderManifest` |
| auth | `token_endpoint_secret` (both call sites pass only `self` fields), `parse_callback_url`, `oauth_request_from_config`, three duplicated `From<RequestTokenError>` impls | zero-arg `&self` method, `LoopbackCallback::parse`, `OAuthRequest::from_mount_config`, one generic mapper; `client.rs` splits into client/loopback/callback/error modules |
| cli | `mount_tree.rs`, `status.rs`, `host_teardown.rs`, `auth/mount.rs`, `setup/host_os.rs` (six fns switching on `HostOs`, no `impl HostOs`), `doctor.rs` (ten probes re-threading ad hoc tuples), `inspector/tree.rs::lookup_node_mut` (sits between two `impl PathNode` blocks) | fold into adjacent impls; `impl HostOs`; a `Doctor` context struct |
| sdk | `collection_to_dir_projection`, `ServeCtx` fns, `object`/`file_object` builders, `pattern.rs` segment fns, `load_from_response` | `Collection::into_dir_projection`, §7 moves, `ObjectHandle::new`, `PatternSegment` methods, `Load::from_response` |
| engine::cache, api, daemon, nfs, fuse | `make_key`/`kind_prefix`/`decode_object`; `parse_record`/`serialize_record`; `install_signal_handler`/`mount_health`/`openapi*`; `read_mount_states`/`write_state`; `new_notifier_handle` (blocked by a bare type alias) | methods on `Key`/`RecordKind`/`StoredObject`, `InspectorRecord`, `Daemon`/`SubsystemHealth`, `NfsMountState`/`StateFile`, a `NotifierHandle` newtype |

Duplicate-helper deletions ride the same slices: the second `split_parent_leaf` inside omnifs-fuse, the hand-rolled hex loop in cache (`hex` is a workspace dep), the duplicated `is_false` serde helper, the two divergent `display_path`s, three hand-rolled poll-until loops (one bounded helper). Three god functions with named seams: `Materializer::apply` (193 lines, split per effect kind), `InitArgs::run_in_workspace` (152, split resolve-provider / create-spec / resolve-auth / apply-live), `endpoint_derive_impl` (174, falls out of the macro unification). `omnifs-cache`'s `anyhow` returns become a `thiserror` error in the merge (the one real anyhow-in-a-library instance).

---

# Part two: delivery

**Execution authority note:** the step-by-step, agent-executable form of this delivery plan (plus the truth track and strategy build-out) lives in [bluesky-execution.md](bluesky-execution.md), which carries the status ledger, per-step gates, and stop conditions. Where sequencing here and there disagree, the execution plan wins. The workstreams below remain the design-level grouping.

## Workstreams and slices

Gates for every slice: committed on `main`; a non-vacuous structural assertion (a named `cargo tree` edge check, zero-hit grep, or visibility/type assertion) passes by exit code; the kernel-free conformance harness is green; authoring-surface slices also pass the tracer providers (GitHub, Linear, Docker) plus live smoke. FUSE- or mount-table-touching slices are verified on a Linux build host; a macOS host gate compiles them only vacuously and false-greens.

### WS0: regression net (first, unblocks everything)

Characterization coverage for behaviors later slices move: exhaustive listing through pagination, effect ordering and invalidation-deletes-durable-state, live growth (`tail -f`, learned sizes), OAuth loopback listener (bind, CSRF, port), inspector redaction, the reconcile state machine; plus two new nets: an auth-lifecycle itest against the fake OAuth server (expiry → refresh → rejection → `NeedsConsent`) and a frontend concurrency check (a slow provider read must not block a second path). Land the three completed deletions from `raulk/phase-a-deletions` (the `Workspace<Role>` phantom, dead view-cache methods, the empty inspector allowlist).

### WS1: consolidation (the topology, mostly mechanical)

In order, each its own PR: (a) `omnifs-api` absorbs `omnifs-inspector`; (b) extract `omnifs-mtab` and delete the four duplicated copies it replaces; (c) merge `omnifs-provider` + `omnifs-mount` + `omnifs-creds` + `omnifs-home` into `omnifs-workspace`, moving core's `auth.rs`, `mount.rs`, and `provider.rs` types in (utoipa behind a feature); (d) merge `omnifs-host` + `omnifs-tree` + `omnifs-cache` + core's `view.rs` into `omnifs-engine`, tightening visibility as the merge lands (`wit_protocol`, pagination, `Materializer` projection methods, cache schema to `pub(crate)`); (e) `omnifs-auth` retargets onto workspace, dropping its mount/provider edges; (f) `omnifs-nfs` sheds its test-only cache dependency in (d). Gates: `cargo tree` asserts the guest set (`omnifs-sdk` and providers depend on exactly core/wit/macros), auth has no engine edge, frontends have no wit edge; a visibility assertion that `omnifs_engine`'s public items are the curated surface (engine construction, `Tree`/`ServingContext`, render, health, events); conformance plus a live `just dev` smoke after (c) and (d). AGENTS.md orientation, the contracts' code listings, and `docs/architecture` crate references update inside each merge PR.

### WS2: concept and naming unification

`Canonical` role-naming; `Backing` renamed up to `Subtree` (Linux-verified); `representation`/`computed`; the post-merge renames of §6 (`MountRuntimes`, `EffectApplier`, `Expiry`, qualified error enums); `Spec.mount: mount::Name`; tagged `Grant<T>`. One contract-touching slice: the `Need` → `AccessNeed`/`ResourceLimit` split (WIT + macro + manifest + all providers, one PR). Strict `Spec` parsing lands here (gated; this document is the surfacing).

### WS3: the auth subsystem (after WS1 slices c and e)

Staged, each shippable: (1) `CredentialService` owning store access, the single expiry function, and the single key derivation, with engine and CLI rewired as callers and behavior unchanged; (2) the proactive refresh loop, health table, and mount-start validation; (3) `report_rejected` replacing engine's retry decision, `invalid_grant` → `NeedsConsent`; (4) revocation wired to `mounts rm`/`reset`; (5) the device-poll compat flag replacing the GitHub shim (provider manifest change, tracer-gated); (6) manifest-declared ambient sources replacing the hardcoded detect match. Gates: the WS0 auth itest at every stage; greps assert `OAUTH_REFRESH_WINDOW`, `oauth_entry_is_fresh`, and the hand-copied `DEFAULT_ACCOUNT` are gone; credential types still never derive `ToSchema`.

### WS4: API v2 (slices 1-4 independent; credential slices after WS3 stage 2)

(1) Structured `ApiError` and typed failure kinds on existing routes; (2) mount CRUD with daemon-side `Registry` writes and the Registry advisory lock in the same slice; (3) daemon-enforced upgrade consent, with `UpgradePlan::diff` full-surface binding as its own prior slice; (4) widened `DaemonStatus`/`DaemonBackend` identity; (5) credential health and reload endpoints; (6) providers endpoint; (7) scoped reconcile and the busy signal; (8) control-port bearer token (gated; confirm before landing). `API_MAJOR` bumps once at the first incompatible slice; `just openapi` plus the parity test gate every slice; `docs/contracts/50-control-plane.md` updates with slice 2.

### WS5: CLI restructure (after WS4 slices 2 and 4)

`daemon_addr` relocation; backend-identity collapse; `Registry`-bypass fixes; command-surface changes (`mounts add` removal, `mounts reauth`, `--json`, doctor live probe); `session.rs` dissolution; readiness delegation to the auth service; the CLI rows of §9; `thiserror` used or dropped.

### WS6: config unification (independent; interleaves from the start)

The §4 items not carried by other streams: `apply_provider_metadata` as sole inheritance implementation; atomic-write standardization; the precedence helper; single-owner constants; inject-domain coverage validation; the guest-path CI grep; Dockerfile de-duplication; `dev.ts` through the CLI.

### WS7: projection and frontends (FUSE async is the priority item)

(1) FUSE async-first dispatch with the in-flight budget (Linux-verified; a live responsiveness bug); (2) `engine::render` with `IdentityTable`/`FollowSizeTable`/invalidation consumer/`retry_class`/attr classification, both frontends migrated with a recorded field-by-field `NodeEntry` diff; (3) the shared materialize cap in `Tree::read`; (4) pagination's projection face into `engine::tree`; (5) `ServingContext` and the death of `Mounts::Single`; (6) dead `cached_file_attrs`/`publish_file_attrs` removal and the `Tree::list` contract note. Gates: the WS0 concurrency check; NFS loopback-bind, FUSE notify, and `tail -f` live growth stay green; live-runtime smoke.

### WS8: SDK and macros (independent; tracer-gated throughout)

`DirProjection` removal with Linear on `Collection<Issue>`; two-stage registration; the `router/object.rs` split, `ServeCtx` methods, typed `ObjectKind`; macro Args-struct unification; `helpers.rs` dissolution; the pattern-module error type.

## Sequencing

WS0 first. WS1 next and early: it is mechanical, it de-risks everything after it, and later streams write into the merged homes instead of paying the old boundaries. WS2 follows WS1 (renames land in final homes). WS3 needs WS1 (c, e); WS4 slices 1-4 can start after WS0 and run beside WS1 where files do not overlap; WS4 credential slices need WS3 stage 2; WS5 trails WS4. WS6, WS7, WS8 interleave freely, except WS7 slice 1 (FUSE async) should land as early as possible. Provider tracers ride every authoring-surface slice.

## Corrections to previous revisions

Recorded so the reasoning is not re-litigated:

- **"Crate count roughly flat; the structure is mostly sound" was the original plan's premise and it was wrong.** The audited violations are code tunneling through walls the domain does not have; the previous revision of this document repaired those edges and added three crates (`omnifs-view`, `omnifs-auth-model`, plus mtab), treating symptoms. The consolidation dissolves two of the three (view becomes `engine::view`; auth-model becomes workspace vocabulary) and keeps only `omnifs-mtab`, which replaces real cross-crate duplication.
- **"The CLI is large but mostly justified" was wrong**: inverted fact ownership, five backend representations, a stated-invariant violation, regrown duplication (§8).
- **The `callout_*` one-liners are not vestigial** (~40 call sites); deletion became optional consolidation.
- **The `Artifact` collapse is decided**: delete the host wrapper, keep `omnifs_provider::Artifact` (post-merge: the workspace `Artifact`).
- **`omnifs-home` does not fold into core**; it folds into workspace, where the layout roots the formats.
- **Driving `omnifs-embed-metadata` from `cargo metadata` is dropped**: the hand list pairs a wasm filename with a compile-time accessor symbol, is parity-tested, and codegen to replace it is negative value.
- **The API was underscoped as "the justified micro-crate"**; §3 is the requirement set the CLI's side channels evidence.
- **Auth was underscoped as a dependency-direction fix**; §2 is the subsystem redesign the reactive evidence demands.

## Risks and non-goals

- **Merge discipline.** WS1's merges are wide but mechanical; each lands alone, with no functional change riding a merge PR, and rebases stay frequent. Naming slices (WS2) do not run concurrently with a merge touching the same types.
- **The macOS false-green trap.** FUSE and the mount-table code are `cfg(linux)`; slices touching them gate on a Linux build.
- **Engine visibility is the new wall; guard it.** A slice that makes an engine internal `pub` for a frontend's convenience is recreating the tunnel; the WS1 visibility assertion stays in CI permanently.
- **Cross-process spec writes.** WS4 slice 2 introduces the daemon as a second Registry writer; the advisory lock lands in the same slice, not after.
- **`ServingContext` must not quietly grow policy**; scope or audit rules graduate it to `Worldview` with enforcement on every serving path, or they do not land.
- **Auth changes are fail-closed.** Every WS3 stage preserves: no silent unauthenticated traffic for a required credential, no secrets on the wire, store permissions unchanged.
- **Non-goals**, unchanged: writes-as-transactions, provider-push liveness, per-consumer scoping (`Worldview`), agent-legibility files, provider-contract versioning (relevant only once providers ship out of tree), metrics endpoints ahead of cache counters. None is scaffolded with empty seams.
