# Provider SDK contracts

Status: current-contract
Owns: provider shape, object model, route dispatch, provider metadata emission, host resource config fields, endpoint values, and WIT-facing SDK changes.

## Read when

Read this before touching `crates/omnifs-sdk`, SDK macros, providers, `provider.wit`, route registration, object blocks, provider metadata, provider config metadata, endpoint helpers, or all-provider migrations.

## Rules

### Provider shape

A provider is one `#[omnifs_sdk::provider]` implementation with synchronous `fn start` registering routes on a `Router`. Keep the provider path surface visible from `start()`. Use SDK constructs for HTTP, status mapping, caching, retry, and projection plumbing.

Git and blob callouts carry request facts only. The host validates those facts
and owns opaque cache identities, body publication, and rehydration; provider
APIs must not expose cache-key or filesystem-entry parameters.

Repeated provider boilerplate is evidence of a missing SDK construct. Fix the shared construct rather than normalizing local scaffolding across providers.

### Object model

Object reasoning lives SDK-side. Object blocks derive from canonical bytes and should not contain ordinary non-canonical handlers.

Use `r.object::<O>` or `r.file_object::<O>` for object-backed paths. Keep canonical payload decode and render behavior with the object type. Return effects from the operation that earned them.

### Route dispatch

Route dispatch has one owner for precedence. Lookup, listing, read, and open must share route-target resolution rules.

`Router` is the mutable registration builder used only during provider startup. `Router::compile` consumes it exactly once, freezes each alias as an additional mounted face, resolves collections, validates capture compatibility and the complete route surface, synthesizes README routes, and returns `CompiledRouter`, the only type that supports runtime dispatch. Provider initialization publishes state and the compiled router only after both startup and compilation succeed. An optional `#[path_captures]` field may be omitted by a route template so one key type can be reused across related route shapes; non-optional fields remain required at compile time.

Keep `r.dir`, `r.file`, and `r.treeref` as the path-oriented face for non-object routes. Use typed `omnifs_core::path::Path` or parsed segments after parse boundaries. Split and join provider paths as strings only at WIT or display boundaries.

### Provider metadata

Provider manifests are generated from `#[omnifs_sdk::provider]` annotations, each `#[omnifs_sdk::config]` type's static config dialect, and a literal wire-shaped auth JSON declaration. `capabilities(...)` declares authority needs only; scalar ceilings use `limits(...)`. The provider macro assembles one JSON byte array at compile time and emits it as the `omnifs.provider-metadata.v1` custom section. The final component is self-describing before host instantiation, and the host parser owns validation and conversion into `ProviderManifest`.

Every auth injection domain must be covered by a declared domain capability need in the same manifest. Metadata validation rejects a scheme whose `injectDomains` entry is not matched by the provider's domain needs, and the error names the scheme key and domain.

Dynamic domain needs are declared as `domain(dynamic, "...")` and resolve from a mount config field named `domains` with type `Vec<String>`. The resolved values become the host-enforced HTTP allowlist for that mount. Literal domain needs remain `domain("host.example", "...")`.

Use `just build providers` when artifacts need embedded metadata and validation-ready Wasm; the provider macro embeds the section during the Wasm build. The host reads the section pre-instantiation, so it never instantiates a component to obtain metadata.

### Host resource config fields

Config fields that name host resources use typed `HostFile` or `HostSocket` fields. The manifest records them as string fields with a host-resource binding, and startup requires the matching dynamic manifest need before resolving exact authority.

Keep host-resource bindings directly on the config field metadata. A matching dynamic `PreopenedPath` or `UnixSocket` need and bound config field resolve exact startup authority once, before provider instance construction; a missing or unpaired declaration fails closed rather than being resolved lazily during provider execution.

### Endpoint values

Endpoint values are intentional. Static endpoints are zero-sized values such as `.endpoint(GitHubApi)`, and runtime endpoints can carry config such as Docker, DNS-over-HTTPS, or Kubernetes base URLs.

Prefer typed endpoints for upstream APIs. Keep endpoint hooks with the endpoint type. Use raw `cx.http()` only when the URL is fully dynamic or the endpoint model does not fit.

### Async host imports

Provider handlers are async, and SDK callout futures await WIT async host imports directly. Do not recreate SDK-side yielded-callout queues, pending future tables, or provider continuation exports.

Use `cx::join_all` for independent concurrent callouts. It starts sibling async imports by polling them in one provider operation; it must not depend on positional host resume batches.

The provider macro owns WIT export glue. Namespace and notify exports are async and return operation-specific typed result/effects tuples. Lifecycle exports return their operation-specific typed result/effects tuples as well.

### WIT coordination

Changing the `Object` trait, route faces, dispatch, provider macro surface, or WIT contract is usually an all-provider migration. Keep providers, SDK tests, WIT boundary tests, and docs in step in the same change.

Each lifecycle, namespace, and notify export returns only its operation-specific typed result together with terminal effects; impossible cross-operation results are unrepresentable at the component interface. Effects remain the only terminal host-mutation channel: provider errors carry empty effects, and the host validates typed success payloads plus effects before committing once.

## Must not

- Hide the main route topology behind one-caller registration helpers.
- Add product-provider fake transports or in-crate callout tests when host, SDK, fixture, or live runtime tests can exercise the behavior.
- Reach past the SDK for host effects unless the SDK is being fixed in the same change.
- Put ordinary file or directory handlers inside object blocks.
- Add a second route shape just to gain access to effects.
- Copy static sibling, object leaf, capture, or implicit-prefix precedence across operation-specific dispatch paths.
- Treat path-oriented routes as inferior escape hatches when the domain is not object-shaped.
- Create or edit `providers/*/omnifs.provider.json`.
- Make the SDK metadata types serialize themselves, or hand-write the metadata JSON dialect; conversion to the host `ProviderManifest` is the harvester's job.
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
- `crates/omnifs-sdk/src/config_resource.rs`
- `crates/omnifs-sdk/tests/wit_boundary.rs`
- `crates/omnifs-sdk/src/metadata.rs`
- `crates/omnifs-workspace/src/provider/sections.rs`
- `crates/omnifs-engine/src/authority.rs`
- `providers/*/src/lib.rs`
- `providers/DESIGN.md`
- `skills/omnifs-provider-sdk/SKILL.md`

## Validation

- `just check providers`
- `just build providers`
- `just validate providers`
- Provider initialization/compilation tests after route-surface changes.
- WIT-boundary tests for object, collection, file-object, preload, effects, `ByteSource`, `DirListing`, and canonical `view_leaves` changes.
- Manifest schema generation/checks when provider config metadata changes.
