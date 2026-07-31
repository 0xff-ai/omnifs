# Provider SDK contracts

Status: current-contract
Owns: provider shape, objects, dispatch, metadata, host-resource config,
endpoints, and WIT-facing SDK changes.

## Read when

Read this before changing the SDK, provider macros or implementations, WIT,
routes, objects, metadata, config, endpoints, or an all-provider surface.

## Rules

### Provider shape

A provider is one `#[omnifs_sdk::provider]` implementation whose synchronous
`start` registers its visible path surface on a `Router`. Use SDK constructs
for HTTP, status mapping, caching, retry, and projection plumbing.

Git and blob callouts carry request facts only. The host validates those facts
and owns opaque cache identities, body publication, and rehydration; provider
APIs must not expose cache-key or filesystem-entry parameters.

Repeated provider boilerplate calls for an SDK fix, not copied scaffolding.

### Object model

Object reasoning stays SDK-side. Use `r.object::<O>` or
`r.file_object::<O>` for object-backed paths; keep canonical decode and render
with the object type and return effects from the operation that earned them.
Object blocks contain no ordinary non-canonical handlers.

### Route dispatch

Route dispatch has one owner for precedence. Lookup, listing, read, and open must share route-target resolution rules.

`Router` is a startup-only builder. `Router::compile` consumes it, freezes
mounted aliases, resolves collections, validates captures and the route
surface, synthesizes README routes, and returns the only dispatchable type,
`CompiledRouter`. Initialization publishes only after startup and compilation
succeed. Route templates may omit optional `#[path_captures]` fields;
non-optional fields remain required.

Keep `r.dir`, `r.file`, and `r.treeref` for non-object routes. Use typed `Path`
or parsed segments internally; split or join strings only at WIT or display
boundaries.

### Provider metadata

Provider annotations, config dialects, and literal wire-shaped auth JSON
generate one `omnifs.provider-metadata.v1` custom section. `capabilities(...)`
declares authority; scalar ceilings use `limits(...)`. The component is
self-describing before instantiation, and the host owns validation and
conversion to `ProviderManifest`.

Every auth injection domain needs a matching domain capability in the same
manifest. Validation names the scheme and unmatched domain.

`domain(dynamic, "...")` resolves from a `domains: Vec<String>` config field
into the mount's HTTP allowlist. Literal needs use
`domain("host.example", "...")`.

`just build providers` emits metadata-bearing, validation-ready Wasm. The host
reads metadata without instantiating the component.

### Host resource config fields

Host-resource config uses typed `HostFile` or `HostSocket` fields with direct
metadata bindings. A matching dynamic `PreopenedPath` or `UnixSocket` need and
bound field resolve exact authority before instance construction; missing or
unpaired declarations fail closed.

### Endpoint values

Static endpoints are zero-sized values such as `.endpoint(GitHubApi)`;
runtime endpoints carry config such as a base URL.

Prefer typed endpoints and keep hooks with their type. Use raw `cx.http()` only
for fully dynamic URLs or a genuine model mismatch.

### Async host imports

Provider handlers await WIT async imports directly. Do not recreate yielded
queues, pending tables, or continuation exports.

Use `cx::join_all` for independent callouts. It polls sibling imports within one
operation and never depends on positional resume batches.

The provider macro owns WIT export glue. Namespace and notify exports are
async; every export returns its operation-specific typed result and effects.

### WIT coordination

Changes to `Object`, route faces, dispatch, macros, or WIT are usually
all-provider migrations. Update providers, SDK and WIT tests, and docs together.

Each export admits only its own result and terminal effects, so cross-operation
results are unrepresentable. Effects are the sole terminal mutation channel:
errors carry none, and the host validates success plus effects before one
commit.

## Must not

- Hide the main route topology behind one-caller registration helpers.
- Add product-provider fake transports or in-crate callout tests when host,
  SDK, fixture, or live runtime tests can exercise the behavior.
- Reach past the SDK for host effects unless the SDK is being fixed in the same change.
- Put ordinary file or directory handlers inside object blocks.
- Add a second route shape just to gain access to effects.
- Copy static sibling, object leaf, capture, or implicit-prefix precedence across operation-specific dispatch paths.
- Treat path-oriented routes as inferior escape hatches when the domain is not object-shaped.
- Create or edit `providers/*/omnifs.provider.json`.
- Make SDK metadata types serialize themselves, or hand-write the metadata JSON
  dialect; the harvester converts it to `ProviderManifest`.
- Hide host resource bindings inside type shapes.
- Revive `x-omnifs-init`, guest-path rewriting, or magic `endpoint` field coupling.
- Split endpoint APIs into type-only and value-only variants unless the value model changes.
- Export speculative SDK surface without a current provider or host path that uses it.
- Reintroduce provider-step, continuation exports, or SDK-managed resume queues for provider callouts.

## Code

- `crates/omnifs-wit/wit/provider.wit`
- `crates/omnifs-sdk/src/lib.rs`
- `crates/omnifs-sdk/src/router`
- `crates/omnifs-sdk/src/object.rs`
- `crates/omnifs-sdk/src/endpoint.rs`
- `crates/omnifs-sdk/src/http.rs`
- `crates/omnifs-sdk/src/cx.rs`
- `crates/omnifs-sdk-macros/src/provider_macro.rs`
- `crates/omnifs-sdk-macros/src/config_macro.rs`
- `crates/omnifs-sdk/src/config_resource.rs`
- `crates/omnifs-sdk/tests/wit_boundary.rs`
- `crates/omnifs-provider/src/sections.rs`
- `crates/omnifs-engine/src/authority.rs`
- `providers/*/src/lib.rs`
- `providers/DESIGN.md`
- `skills/omnifs-provider-sdk/SKILL.md`

## Validation

- `just check providers`
- `just build providers`
- `just validate providers`
- Provider initialization/compilation tests after route-surface changes.
- WIT-boundary tests for object, collection, file-object, preload, effects,
  `ByteSource`, `DirListing`, and canonical `view_leaves` changes.
- Manifest schema generation/checks when provider config metadata changes.
