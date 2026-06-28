# Provider SDK contracts

Status: current-contract
Owns: provider shape, object model, route dispatch, provider metadata emission, host resource config fields, endpoint values, and WIT-facing SDK changes.

## Read when

Read this before touching `crates/omnifs-sdk`, SDK macros, providers, `provider.wit`, route registration, object blocks, provider metadata, provider config metadata, endpoint helpers, or all-provider migrations.

## Rules

### Provider shape

A provider is one `#[omnifs_sdk::provider]` implementation with synchronous `fn start` registering routes on a `Router`. Keep the provider path surface visible from `start()`. Use SDK constructs for HTTP, status mapping, caching, retry, and projection plumbing.

Repeated provider boilerplate is evidence of a missing SDK construct. Fix the shared construct rather than normalizing local scaffolding across providers.

### Object model

Object reasoning lives SDK-side. Object blocks derive from canonical bytes and should not contain ordinary non-canonical handlers.

Use `r.object::<O>` or `r.file_object::<O>` for object-backed paths. Keep canonical payload decode and render behavior with the object type. Return effects from the operation that earned them.

### Route dispatch

Route dispatch has one owner for precedence. Lookup, listing, read, and open must share route-target resolution rules.

Keep `r.dir`, `r.file`, and `r.treeref` as the path-oriented face for non-object routes. Use typed `omnifs_core::path::Path` or parsed segments after parse boundaries. Split and join provider paths as strings only at WIT or display boundaries.

### Provider metadata

Provider manifests are generated from `#[omnifs_sdk::provider]` annotations, each `#[omnifs_sdk::config]` type's static config dialect, and static auth metadata. The provider macro lowers that into a `Provider::METADATA` associated const and a non-wasm `provider_metadata()` accessor. The native `omnifs-embed-metadata` build tool links the provider crates, converts each const into the host `ProviderManifest`, serializes it with `serde_json`, and injects the JSON as the `omnifs.provider-metadata.v1` custom section. The host (and `omnifs-auth`) own that JSON dialect; the SDK metadata types are typed const data and never serialize themselves.

Use `just providers build` when artifacts need embedded metadata and validation-ready Wasm; it runs the harvester after the Wasm build. The host reads the section pre-instantiation, so it never instantiates a component to obtain metadata.

### Host resource config fields

Config fields that name host resources use typed `HostFile` or `HostSocket` fields. The manifest records them as string fields with a host-resource binding, and the host resolves those grants at mount-start.

Keep host-resource bindings directly on the config field metadata. Resolve dynamic grants during mount materialization, not lazily during provider execution.

### Endpoint values

Endpoint values are intentional. Static endpoints are zero-sized values such as `.endpoint(GitHubApi)`, and runtime endpoints can carry config such as Docker, DNS-over-HTTPS, or Kubernetes base URLs.

Prefer typed endpoints for upstream APIs. Keep endpoint hooks with the endpoint type. Use raw `cx.http()` only when the URL is fully dynamic or the endpoint model does not fit.

### Async host imports

Provider handlers are async, and SDK callout futures await WIT async host imports directly. Do not recreate SDK-side yielded-callout queues, pending future tables, or provider continuation exports.

Use `cx::join_all` for independent concurrent callouts. It starts sibling async imports by polling them in one provider operation; it must not depend on positional host resume batches.

The provider macro owns WIT export glue. Namespace and notify exports are async and return terminal `provider-return` values. Lifecycle exports remain terminal.

### WIT coordination

Changing the `Object` trait, route faces, dispatch, provider macro surface, or WIT contract is usually an all-provider migration. Keep providers, SDK tests, WIT boundary tests, and docs in step in the same change.

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
- `crates/omnifs-embed-metadata/src/main.rs`
- `crates/omnifs-provider/src/sections.rs`
- `crates/omnifs-mount/src/materialize.rs`
- `providers/*/src/lib.rs`
- `providers/DESIGN.md`
- `skills/omnifs-provider-sdk/SKILL.md`

## Validation

- `just providers check`
- `just providers build`
- `just providers validate`
- Provider initialization/seal tests after route-surface changes.
- WIT-boundary tests for object, collection, file-object, preload, effects, `ByteSource`, `DirListing`, and canonical `view_leaves` changes.
- Manifest schema generation/checks when provider config metadata changes.
