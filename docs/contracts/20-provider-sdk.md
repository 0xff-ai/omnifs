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

### Page validators

A listing page may carry a `PageValidator`: an opaque token, such as the `ETag` of its upstream answer, with the seconds of the interval its upstream asks between requests. A collection gives it with `Collection::with_validator` and a dir handler with `DirListing::with_validator`. Once the page is past its deadline, the host sends the token back as the `validator` of `list-children` for the cursor of that page; a collection reads it with `ListCx::validator` and a dir handler with `DirCx::version`, and each answers `Collection::unchanged` or `DirListing::unchanged` while the upstream still matches it, with the validator that the page keeps from then on. The validator reaches only the owners of a page, a dir route and the collection of an object anchor, never the anchor object's own load, which an unchanged page answers without. `RequestBuilder::page` is the conditional terminal of a page: `304` answers unchanged, and `200` the decoded body with the `ETag` of the answer as its validator, both with the headers of the answer.

### Feeds

An object whose upstream reports its changes keeps the facts of its fed faces in the host projection through one feed. `o.feed(method).starts([...])` declares the feed with its starting paths relative to the object (`""` names the anchor itself), and the feed answers for the fed paths of the object's subtree. `.fed()` on a file face feeds its lookup and its bytes, on a dir face its listing, and `o.fed()` the lookup of the anchor, in place of a computed, canonical, or collection face; the listing of a fed anchor lists its faces without a load of its object. `Router::compile` checks each starting path against a fed face of its object's subtree and gives each fed face its claims: the feeds of its ancestry, the outer object first.

An operation on a fed path answers `feed(claims)` without a request, with `starts` for an operation on a starting path; the host decides which feed answers and when. The `feed` export runs the feed of the object route at its `path` with a `FeedCx`: that anchor path, which the paths of the facts extend since a key may normalize a capture, the key, the token the host kept, the declared starting path of the operation that started the answer, `FeedCx::listing` over the projection, and the facts of the part, stable inline files and directories with a `ListingWrite` (`Partial`, `Exhaustive`, or `Complete`) written at mount-absolute paths, and removals. The method answers `Feed::unchanged()` or `Feed::part(token)` with `more` and the inner tokens of the objects of inner feeds by object type and key. The provider macro generates `feed` for every provider; a path without a feed is not found.

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
- `crates/omnifs-sdk/src/feed.rs`
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
- WIT-boundary tests for object, collection, file-object, preload, effects, `ByteSource`, `DirListing`, page validator, and canonical `view_leaves` changes.
- Router tests for feed declarations, fed-face claims, and the `feed` export after feed changes.
- Manifest schema generation/checks when provider config metadata changes.
