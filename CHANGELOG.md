# Changelog

All notable changes to this project will be documented in this file.
## [0.3.0] - 2026-07-01

### CLI & workflow
- Recommend auth status in shell banner
- Make authentication self-explanatory and streamline the verb surface (#131)
- Single omnifs binary with docker and host-native daemon backends (#137)
- Embed the provider WASM bundle in the CLI binary (#142)
- Make daemon lifecycle and mount compatibility explicit (#143)
- Open the dev container shell with the image's zsh
- Use host-built provider artifacts (#154)
- Choose the default runtime during setup on any OS
- Add `omnifs up --runtime` for a one-off runtime override
- Install providers on demand and refine setup capability prompts
- Always show init capability grants

### Other
- Adopt 0xff-ai/bot for changelog entries (PR-branch + merge gate) (#117)
- Project the tree through a frontend-neutral core, host-native on macOS over NFS (#134)
- Keep arm64 CI on the ubuntu-24.04 blacksmith runner
- Tree/collection-capture/choices faces + migrate GitHub to object SDK
- File_object direct form, BlobFile stability, object stream face; docker/test blockers
- Omnifs dev.
- Run providers on Wasmtime component async (#160)

### Providers & projected paths
- Apply file leaf modifiers to pending leaves (#110)
- Stamp contract evidence and surface unhandled events (#112)
- Read-only Kubernetes provider with throwaway dev cluster (#123)
- Add Oura provider with client-side-token OAuth and preload effects (#113)
- Project already-fetched data into listings (#146)
- Expand default dev profile and add oura mount
- Serve table sample.json as a whole-file body projection
- Object trait reshape + collection/invalidation foundations
- Object SDK core — faces, dispatch, macros, snapshot (gates green, 2 known blockers)
- WIT-boundary harness + correct collections, file-object preload, leaf resolution
- Migrate remaining 8 providers to object SDK (wasm-green, known fixups)
- Mount Paper at /papers/{paper}/{version}, not root; add all-providers seal gate
- Preloaded + just-read file-object days project as listable dirents
- Author provider manifests from annotations (dns, arxiv)
- Assemble the provider manifest section at build by running the component
- Author db, docker, and kubernetes manifests from annotations
- Author auth-provider manifests from annotations, retire the JSON path
- Infer provider state in macros
- Add explicit artifact installs (#157)
- Add path segment macro (#158)

### Runtime & mounts
- Preserve learned file sizes (#106)
- Live tail -f follow for volatile files (#115)
- Add read-only NFSv4 loopback frontend to the daemon (#126)
- Reconcile mounts from disk and own the runtime lifecycle (#141)
- Host-enforced capability model and content-addressed provider pinning (#145)
- Publish reconcile replacements after build
- Pass container-native preopens through materialization
- Make a spec file's name its mount identity, and route reset through the Registry
- Keep the mount responsive under slow and throttled providers
- Defer slow provider READDIR with NFS4ERR_DELAY
## [0.2.1] - 2026-06-08

### Fixed

- `omnifs up` now passes the runtime container's config, cache, mount-config, and provider directories through `OMNIFS_*` environment variables. This keeps the startup readiness check aligned with the materialized session mounts, so release containers no longer report zero providers after the FUSE mount is already live.

## [0.2.0] - 2026-06-08

### Added

- Object-shaped provider routing and host-owned object/view caching now back the current provider surfaces. Providers can store canonical upstream bytes and render multiple filesystem leaves from the same object, so repeated reads of related fields can be served from host cache instead of refetching the upstream service.
- `~/.omnifs/config.toml` is now the normal user-authored configuration surface for mounts. `omnifs init` writes inline `[[mounts]]` entries, and mount discovery still reads legacy per-mount JSON files so existing setups continue to load.
- The public README is rewritten for launch around the shipped alpha surface: six live providers, the npm + runtime-container install path, Linux FUSE caveat, host-mediated provider authority, and the read-only roadmap boundary.

### Changed

- Provider paths now use plain resource names instead of underscore-prefixed control names. GitHub exposes `repo`, `issues`, `pulls`, and `actions`; filters are `open` and `all`; DNS exposes `resolvers`, `reverse`, `all`, and `raw`; Docker exposes `/containers.json`, `/compose.json`, and `{by-name,by-id,running,stopped}` under `/containers`.
- Built-in providers have been rewritten against the current SDK registration model, removing migration-era provider boilerplate while preserving the shipped GitHub, DNS, arXiv, Docker, Linear, and SQLite path surfaces.
- The default credential backend is now the private `credentials.json` file. The keychain backend remains available through `OMNIFS_CREDS_BACKEND=keychain`, but normal startup, auth, and runtime refresh paths no longer trigger platform keychain prompts by default.

### Fixed

- CI host-test Wasmtime cache restore now uses stable OS/architecture restore keys with per-run write keys, improving cache reuse between push and pull-request runs without depending on synthetic merge SHAs.

## [0.2.0-dev.2] - 2026-05-28

### Added

- Container shell now greets interactive users with a welcome banner: an "OMNIFS" wordmark, the tagline `open a path, read the world.`, and indented blocks of example paths (`ls /github/<owner>/<repo>/repo`, `cat /dns/<domain>/TXT`, an arXiv `find` pipeline) and useful commands (`omnifs status`, `omnifs logs -f`, `omnifs auth status`). Gated on `[[ -o interactive ]]` so `zsh -c '...'` invocations stay silent. Lives in `scripts/container-zshrc.zsh`, copied into both the dev `Dockerfile` and the release `scripts/ci/Dockerfile.runtime`.
- `omnifs up` now prints a hint pointing at `omnifs shell` after the FUSE mount comes online, so new users immediately know how to enter the projected filesystem.
- `omnifs inspect` shows a live JSONL observability stream from the host daemon as a ratatui TUI: a path-tree of mount activity, a per-mount sparkline strip, an operations log (retention 4096), and an inline waterfall for the selected trace. Use `--replay <file>` for a captured JSONL trace, `--record <file>` to tee the live stream to a host path while attached, and `--plain` for line-oriented output. The host daemon emits typed `InspectorEvent` records (FUSE, provider, callout, subtree, clone, cache) through a non-blocking sink (lock-free `crossbeam_queue::ArrayQueue` history ring plus `tokio::sync::broadcast` for live subscribers) over TCP loopback `127.0.0.1:7878`. Schema lives in the new `omnifs-inspector` crate with redaction at the wire boundary. File tee is opt-in via `OMNIFS_INSPECTOR_PATH`. `omnifs up` exposes the inspector port to host loopback so `omnifs inspect` works against the standard runtime container, not just `omnifs dev`.
- Runtime images now carry an `ai.0xff.omnifs.min-launcher-version` label, and `omnifs dev` refuses to start an image that requires a newer launcher. This catches source-image versus installed-CLI skew before Docker starts a container with missing port mappings or environment wiring.
- `OMNIFS_CREDS_BACKEND=file|keychain` overrides the automatic credential backend selection, and `omnifs dev` uses the file backend directly so contributor builds do not block on macOS Keychain prompts from differently signed binaries.

### Changed

- Repository moved from `raulk/omnifs` to `0xff-ai/omnifs`. All repository URLs, GHCR image references (`ghcr.io/0xff-ai/omnifs`), npm package homepage/repository metadata, README and documentation links, and the `omnifs dev` clone hint are updated. The `OMNIFS_DEMO_OWNER` default in `scripts/demo.sh` now points to `0xff-ai`. npm package names are unchanged (`@0xff-ai/omnifs` and `@0xff-ai/omnifs-cli-*`).
- `omnifs setup` provider multiselect now sorts `db` and `linear` to the bottom and starts them unchecked. Both require user-supplied state (a SQLite fixture path, a Linear API key) that the smoke onboarding flow can't satisfy from ambient context.

### Fixed

- `omnifs inspect` now reports honest socket state. The TUI starts in a waiting state until the daemon actually accepts a TCP connection, and `--plain` mode prints connect, disconnect, and delayed "no inspector listening" diagnostics instead of silently reconnecting forever.
- GitHub repository names such as `.github` and `.gitignore` are accepted as safe path segments. Only bare `.` and `..` remain rejected for traversal safety.
- GitHub issue listings under a nonexistent repository now surface `NotFound` instead of `InvalidInput`, so FUSE renders `ENOENT` rather than `EINVAL` for structurally valid paths that point at a missing repo.
- GitHub OAuth device flow no longer fails on the first poll with `request_failed: Failed to parse server response`. GitHub's `/login/oauth/access_token` returns `200 OK` with `{"error":"authorization_pending",...}` while the user is approving, violating RFC 8628; `oauth2` 5.x interpreted the 200 as success and tripped on the body schema. `omnifs-auth` now wraps `reqwest::Client` in a `DevicePollingHttp` impl of `oauth2::AsyncHttpClient` that re-stamps any `200 + JSON-with-error-field` response as `400`, routing GitHub's responses through the standard error-handling path. Compliant providers are unaffected.
- `omnifs dev` builds again. The `Dockerfile` still referenced `.cargo/`, which was removed in #78; the stray `COPY .cargo .cargo` and matching `.dockerignore` allowlist entries are gone.

## [0.2.0-dev.1] - 2026-05-26

### Fixed

- `omnifs` CLI shim no longer fails at startup with `Cannot find module '../../platforms.json'`. The platform-to-package map was previously loaded at runtime from `npm/platforms.json`, which lives outside the published `@0xff-ai/omnifs` tarball and is not included on install. The map is now inlined in `scripts/resolve-binary.js`, with `just npm-validate` cross-checking it against `npm/platforms.json` so the two cannot drift.

## [0.2.0-dev.0] - 2026-05-26

### Added

- omnifs is now distributed on npm as `@0xff-ai/omnifs`. Install with `npm install -g @0xff-ai/omnifs`; the CLI binary ships in one of four platform-specific optional-dependency packages (`darwin-arm64`, `darwin-x64`, `linux-arm64`, `linux-x64`). The Docker image is pulled on `omnifs up`, not at install time.
- Full `omnifs init <provider>` / `omnifs up` / `omnifs down` mount lifecycle. `omnifs init` walks through provider auth (device-code or PKCE loopback OAuth), writes mounts into `~/.omnifs/config.toml`, and stores credentials in the OS keychain (macOS Keychain, Linux libsecret, Windows DPAPI) with a mode-600 file fallback at `~/.omnifs/credentials.json`. `omnifs up` pulls the matching runtime image, materialises credentials into a private session directory bind-mounted read-only into the container, then removes the session on stop or failure.
- `omnifs auth` subcommands: `login`, `logout`, `status`, `refresh`, `scopes`, `import`. OAuth refresh happens automatically with a one-shot 401 retry coordinated by a singleflight plus a cross-process file lock so concurrent CLI invocations do not race on the same refresh token.
- `omnifs setup` guided first-run walkthrough: detects OS and Docker, helps pick providers, runs `init` for each, and brings the container up.
- `omnifs doctor` runs ten ordered probes to diagnose why a mount is not working (Docker availability, FUSE timeout, missing credentials, etc.).
- `omnifs reset` clears configs and credentials after an explicit confirmation prompt.
- `omnifs mounts ls` / `omnifs mounts rm` for listing and removing configured mounts.
- `omnifs status` readiness card showing runtime state, configured mounts, and auth state.
- `omnifs version` and `omnifs completions` commands.
- `omnifs dev` contributor sandbox: walks up from cwd to find the workspace `Cargo.toml`, captures `gh auth token`, downloads the Chinook SQLite fixture into `.secrets/db/test.db`, builds an `omnifs:<short-sha>-dev` image, and starts a container with all built-in providers mounted. Replaces the old `just dev` / `docker compose up` workflow.
- Three supported OAuth flows in the new `omnifs-auth` crate: PKCE loopback (browser-redirect), PKCE manual code (paste-back), and device code (visit URL, type short code). GitHub uses device code with no default write scopes; Linear uses PKCE loopback with the `read` scope.
- Host-managed provider credentials: providers never see tokens; the host attaches them to outgoing HTTP requests after callouts cross the WASM boundary. Provider auth needs are declared in `omnifs.provider.json` (OAuth endpoints, scopes, injection header, allowed domains); adding a new service does not require patching the host.
- `omnifs-creds` crate for the keychain + file + in-memory credential store, with `CredentialKey::storage_key()` (`provider:scheme:account`) as the public wire form. Stale file-fallback entries are cleaned up after successful keyring writes; durability and permissions are hardened.
- Database provider (`omnifs-provider-db`) mounted at `/db`, projecting a read-only SQLite database as a filesystem. Exposes `meta/{version.txt,path.txt,info.json}` and `tables/{name}/{schema.sql,schema.json,indexes.json,count.txt,sample.json}`. SQLite runs inside the WASM sandbox via `rusqlite` with the bundled feature; the host preopens the database file's parent directory through Wasmtime's WASI context with read-only permissions so no bytes cross the WIT boundary. v1 is SQLite-only and read-only.
- Docker provider (`omnifs-provider-docker`) mounted at `/docker`, projecting the local Docker daemon over the Unix socket. Exposes `/system/{info,version,df}.json`, `/system/ping`, `/containers.json`, `/compose.json`, facets `{by-name,by-id,running,stopped}` under `/containers` each binding to a per-container subtree (`inspect.json`, `summary.json`, `summary.txt`, `state`), and `/compose/{project}/services/{service}/containers/{name}` grouping by Compose labels. Container state files are marked volatile so reads bypass the kernel page cache and always reach the provider. Bounded-window `/events` polling translates container actions into cache-invalidation prefixes.
- Unix-socket HTTP transport in the host. Providers use `HttpEndpoint::Unix` and `build_url(path, query)`; the host detects `unix:` URLs, decodes the hex-encoded socket path, builds a per-socket `reqwest::Client` via `ClientBuilder::unix_socket`, and rewrites the URL for that client. Unix sockets are gated by a new `unix-sockets` allowlist on `CapabilityGrants`.
- Linear provider (`omnifs-provider-linear`) mounted at `/linear`, projecting a Linear workspace. Teams appear at `/linear/teams/{KEY}`, issues at `/linear/teams/{KEY}/issues/{open,all}/{IDENT}/`. Each issue surface has `title`, `state`, `priority`, `assignee`, and `description.md` as files. Issue listings preload child files so a `cat` after `ls` skips a follow-up round trip. Uses Linear's GraphQL API with hand-written query strings and serde response structs (Linear's endpoint rejects full introspection queries as too complex, ruling out code generation).
- arXiv provider now also exposes a recent-submissions surface per category: `/categories/{cat}/recent`, `/categories/{cat}/recent/fetched`, `/categories/{cat}/recent/pages/{n}`, and `/categories/{cat}/submissions/{YYYYMMDD}` directories discovered from fetched pages. Direct paper lookup at `/papers/{id}` is unchanged.
- Projected file attributes: providers now declare `Size` (`Exact`, `NonZero`, or `Unknown`), `Bytes` (inline or deferred), `ReadMode`, and `Stability` (`Immutable`, `Mutable`, or `Volatile`) through the `Projection` API. The host uses these facts to set `st_size`, FUSE direct-I/O flags, cache behavior, ranged-read handling, and post-read size promotion. The old 256 MiB placeholder is removed. Volatile files return `entry_timeout = 0`, `attr_timeout = 0`, and `FOPEN_DIRECT_IO`; ranged files open a provider handle for snapshot-consistent reads.
- Sandboxed archive extraction via a host-owned `omnifs-tool-archive` Wasm component. `BlobExecutor` streams `fetch-blob` responses into a staged temp file and commits metadata before the body is visible; `ArchiveExecutor` keys extracted trees by `(cache-key, format, strip-prefix)` and coalesces concurrent extractions through `TreeMaterializer`. The extractor runs path sanitization, depth/length/entry-count/per-file/total-byte limits inside the sandbox and publishes completed trees via atomic directory rename so tree refs never observe partial output.
- `EffectiveConfig` type representing a mount after provider metadata has been merged in. `ProviderCatalog::load_mount()` returns one; credential targeting, session materialisation, and runtime construction all consume it. The previous `InstanceConfig`-plus-late-`apply_metadata` pattern is removed.
- Provider runtime capabilities now come back from `init` as `(State, ProviderInfo, RequestedCapabilities)` instead of a separate WIT export. Initialisation runs exactly once in `ProviderRuntime::new`. Capability entries can be marked `dynamic: true` when the concrete grant depends on mount config (Docker's socket path is the motivating case).
- `omnifs-mount-schema` crate is split into `omnifs-mount` and `omnifs-provider`, with a checked-in provider JSON schema at `crates/omnifs-provider/schema/omnifs.provider.schema.json` (regenerate with `just schema`).
- Per-crate README files for all published crates.

### Changed

- arXiv provider route model is restructured around recent submissions. The calendar/date-query, `new`, `updated`, `by-author`, `/authors`, and `/search` surfaces are removed. Category traversal now goes through `/categories/{category}/recent` and `/categories/{category}/submissions/{YYYYMMDD}`; direct paper lookup at `/papers/{paper}` is unchanged. The only live category listing query shape is `search_query=cat:{category}` sorted by `submittedDate` descending.
- Provider protocol vocabulary is reorganized into three orthogonal channels: `callout` (intermediate host work the provider suspends on), `return` (the completed operation answer), and `effect` (host-side mutation committed at the return boundary). `provider-response` is renamed to `provider-step` with arms `suspended(callouts)` and `returned(provider-return)`. A return cannot carry callouts; an error return cannot carry effects.
- Dead git callouts (`git-list-tree`, `git-read-blob`, `git-head-ref`, `git-list-cached-repos`) and the unused `reconcile` interface are removed from the WIT.
- The `sidecar::materialize` method is folded into `lookup-child` and `list-children` via `#[subtree]` dispatch from the SDK.
- Host runtime module `crates/omnifs-host/src/runtime/mod.rs` is split into focused modules: `instance.rs` (Wasmtime mechanics), `callouts.rs` (dispatch and tracing), `effects.rs` (terminal mutations and `ProjectionAccumulator`), `log_redaction.rs`, `wit_conversions.rs`, `op.rs` (the `Op` enum and `Validator`), and `http_stack.rs` (shared HTTP transport). `RuntimeError` construction errors split into `RuntimeBuildError`.
- Browse cache re-skin enums collapse into their WIT counterparts; `cache::SCHEMA_VERSION` bumps to 5, invalidating existing L2 records.
- CLI flows are redesigned around mount configs, provider metadata, credential materialisation, and container lifecycle commands. Verbose output is off by default; `-v` enables INFO logs and `-vv` adds DEBUG. Common errors surface a `Try:` block with a concrete next step.
- Provider manifests (`omnifs.provider.json`) describe auth schemes, token injection policy, capability grants, and config metadata. All built-in providers (`arxiv`, `db`, `dns`, `docker`, `github`, `linear`, `test`) carry an `omnifs.provider.json` and drop vestigial `[package.metadata.component]` Cargo sections.
- Docker Compose development entrypoints (`compose.yaml`, `just dev`) are replaced by the supported `omnifs dev`, `omnifs shell`, `omnifs logs`, and `omnifs down` workflow.
- Wasm providers are installed into the host `OMNIFS_HOME/providers` directory and the trusted runtime container bind-mounts that home at `/root/.omnifs`.

### Fixed

- Large file content (PR diffs, arXiv papers over the 512 KiB `MAX_EAGER_RESPONSE_BYTES` cap) no longer returns EIO. The GitHub and arXiv providers route oversized reads through `fetch-blob` so bytes stay host-side; the SDK gains `FileContent::Blob` and `FileContent::BlobWithAttrs` variants for blob-backed file content.
- `cd /github/<owner>` followed by `ls` no longer re-fetches the listing. The SDK's `projection_exact_lookup` was marking dirents non-exhaustive even when the handler returned `PageStatus::Exhaustive`; a new `listing-exhaustive` flag on `proj-entry` propagates the exhaustive bit into the host's projection accumulator.
- `lookup_child` into a bind site now dispatches correctly when `parent_path` equals the bind template exactly (not just when it is a strict ancestor). Previously the lookup fell through to the no-handler branch and returned `NotFound`.
- `projection_exact_lookup` was packing the looked-up target's children into `lookup-entry.siblings` instead of the target's actual siblings. The host was caching the listing under the wrong key. The SDK now populates `siblings` with the target's siblings computed from the parent's static children, with the exhaustive bit derived from `StaticChildren::parent_has_dynamic_children`.
- Synchronous FUSE invalidation is removed from the provider callout path, eliminating hangs when reading GitHub projected files such as issue bodies.
- DNS and other unknown-size full-read files now return complete content through `cat`, `head`, and similar tools instead of appearing empty because the kernel saw a zero or one byte sentinel before provider content was materialised.
- Credential persistence no longer leaves stale file-fallback entries after keyring writes; file-store durability and permissions are hardened.
- Host credential-store setup now logs when keyring access falls back to the file store.
- `sdk-macros` dev-dependency on `omnifs-sdk` no longer carries a redundant version specifier, fixing workspace version resolution.

## [0.1.0] - 2026-05-07

### Added

- FUSE filesystem on Linux that projects external services into local paths, with macOS and Windows planned.
- WASM-based provider architecture using the WIT Component Model (`wasmtime`); each provider is a `wasm32-wasip2` component implementing the `omnifs:provider` WIT interface.
- Per-provider capability declarations (HTTP domains, auth types, memory limits, git/websocket/streaming flags) enforced by the host runtime.
- GitHub provider mounted at `/github`, projecting repos, issues, PRs, CI runs, diffs, and source trees as files. Source trees are bind-mounted clones (cloned on demand via SSH); issues and PRs project per-item directories with title, body, state, and comments as separate files.
- Git-backed reconciliation scaffolding via a custom remote helper (read path live; mutation path WIP per the design docs).
- arXiv provider mounted at `/arxiv`, projecting per-paper subtrees (`paper.pdf`, `source.tar.gz`, `metadata.json`, `links.json`, `versions/v{n}/`) under `/arxiv/papers/{id}` and the `/categories/{cat}/{ym|new|updated|by-author}`, `/authors/{author}/{...|by-category}`, and `/search/{query}` scopes.
- DNS provider mounted at `/dns`, projecting record types (A/AAAA/MX/NS/TXT/CNAME/SOA/SRV plus `all` and `raw`) over DNS-over-HTTPS, with resolver-scoped queries via `/dns/@{resolver}/...` and reverse lookups via `/dns/reverse/{ip}`.
- Path-first SDK with attribute macros: `#[dir]`, `#[file]`, `#[treeref]`, `#[bind]`, `#[mutate]` inside `#[handlers] impl ...` blocks; `#[subtree] impl B { ... }` for typed subtree dispatch (mounted at any number of `#[bind("...")]` sites); `#[config]` and `#[provider(mounts(...))]` at the top level. Path patterns support literal segments, bare captures (`{name}`), prefix captures (`v{ver}`), and rest captures (`{*tail}`).
- Auto-navigable intermediate directories: any literal-segment prefix of a registered route is a directory without an explicit handler, so providers don't write no-op stubs for navigation nodes. Listings carry `exhaustive=false` whenever a sibling capture or rest segment lives at depth+1, preventing the host's negative cache from short-circuiting valid dynamic-capture lookups.
- Parse-function fallthrough in route matching: when the highest-precedence pattern's parse function rejects a candidate path, the dispatcher falls through to the next-most-specific candidate. Per-segment validators participate in match candidacy rather than acting as a post-match check.
- Two-tier (L0 in-memory, L2 redb-backed) browse cache with negative caching, projected-file extraction from listings, and `event-outcome` driven invalidation; sibling preload on lookup terminals so the host caches projected siblings alongside the primary entry.
- Cross-listing PR preload and hybrid issue/PR pagination in the GitHub provider.
- HTTP SDK gains POST + raw / JSON bodies and adopts `http` crate request/response types.
- Docker Compose dev workflow (`compose.yaml`, `just dev`, `just shell`) with self-starting published image.
- `docs/design/path-dispatch-and-listing.md` as the perennial reference for routing precedence, listing semantics, and the `lookup`/`readdir` authority split; `docs/design/projected-file-sizes.md` documenting the `direct_io` redesign.

### Changed

- SDK and host runtime are redesigned around path-first handlers and callouts. Effect-based handler signatures are replaced by free-function path handlers that return either a terminal `op-result` or a list of `callout`s for the host to execute and resume. The previous effect-style API is removed.
- Provider configuration moved from TOML to JSON. The host parses each mount's JSON config into `InstanceConfig` and re-serializes the provider-specific `"config"` object as JSON bytes for `initialize()`; providers deserialize via `serde_json::from_slice` (the SDK's `#[config]` macro wires this up automatically).
- Subtree handoff is now declared with `#[treeref("...")]` (renamed from the previous `#[subtree]`) so `#[subtree]` is free for typed-subtree-dispatch impl blocks. The WIT result variant is still spelled `subtree`.
- Dir and file handlers can co-exist on identical rest-captured templates; the parent dir handler's projection authoritatively decides the child's kind at lookup.
- Handler `cx` parameter is now optional, and the `DirCx` lifetime is dropped.

### Fixed

- Bind exact-match lookups no longer return a bare `Lookup::entry` with an implied `exhaustive: true` over an empty sibling set. The host's lookup-side cache treated the bare entry as "the bind has no children" and wrote an exhaustive empty `Dirents` at the bind site, causing subsequent `readdir` to short-circuit before the typed subtree's `list_children` ran.
- Parent listings that returned exhaustive empty no longer poison dynamic-capture child lookups. With the auto-navigable rule and the SDK's exhaustive-flag computation, listings under parents with capture children at depth+1 are correctly non-exhaustive, so the host's negative cache leaves room for the capture handler to dispatch.
- GitHub route projections normalized to honor sibling preload, projected-files, and cross-listing PR preload paths.
