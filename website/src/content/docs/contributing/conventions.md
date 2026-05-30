---
title: Coding conventions
description: omnifs conventions - small local changes, From/TryFrom boundaries, hashbrown maps, CLI vs schema types, protocol guardrails, design judgment, and test quality.
---

These conventions are the working rules for changing omnifs. They favor a small,
direct codebase over abstraction for its own sake. Read them before opening a
non-trivial PR.

## Keep changes small and preserve architecture

- Keep changes small and local.
- Preserve the current architecture unless the task explicitly changes it:
  inode table, router, providers, GitHub cache/scheduler/poller, and clone
  manager.
- Do not silently change the auth model or transport model. If you switch clone
  transport from SSH to HTTPS/token, call it out explicitly - it changes the
  operational contract.
- When a refactor touches clone, routing, or traversal behavior, compare against
  the pre-refactor behavior before accepting the new result.
- Preserve repo-tree passthrough and ownership semantics unless intentionally
  changing the contract.
- Providers must project all data they already have. If a handler holds an
  upstream payload, emit every sibling file and child derivable from it instead
  of returning only the requested field and forcing later refetches.

## Rust type conversions

Prefer `From` and `TryFrom` at type boundaries instead of `foo_to_bar` free
functions when the conversion is a true one-to-one mapping.

Keep free functions when:

- orphan rules block a cross-crate impl (e.g. `credential_entry_from_token` from
  an oauth2 token to `omnifs_creds::CredentialEntry`),
- extra context is required (e.g. `io_context_into(context, err)`,
  `projected_file_from_projection(..., parent, name)`),
- the helper is callout-specific extraction for `CalloutFuture`
  (`fn(CalloutResult) -> Result<T>`) - do not use `TryFrom<CalloutResult>` for
  single-variant unwraps,
- the mapping is conditional or formatting-only (e.g. HTTP `status_error` with
  429 / `retry-after` handling).

When orphan rules block `From<A> for B`, use a local newtype in the owning crate
(e.g. `WitHeaders(&HeaderMap)` in `omnifs-sdk/src/http.rs`) rather than a
conversion helper that hides the same logic.

In `omnifs-sdk`, `Result<T>` aliases `core::result::Result<T, ProviderError>`,
so `TryFrom` impls must return `std::result::Result<_, ProviderError>`
explicitly. Host-only error enums may be supersets of WIT/guest types; map with
`From` at the boundary instead of re-exporting guest bindgen types as the host
public error.

Existing conversion hubs: `host/runtime/wit_conversions.rs`,
`omnifs-sdk/file_attrs.rs`, `omnifs-sdk/browse.rs`,
`host/runtime/{blob,git,archive}.rs`.

## Provider internals

Use `hashbrown::HashMap` for provider-internal maps. It keeps provider internals
predictable across WASI targets.

## CLI presentation vs schema types

- `omnifs-mount-schema` types are wire/config truth. Do not add human-facing CLI
  labels or terminal formatting there.
- Provider capability display uses the schema `CapabilityEntry` enum directly.
  Format at the use site through `crates/cli/src/capability.rs`
  (`capability_label`, `capability_value`). Do not introduce parallel CLI
  view-model structs for the same schema type.
- Status/JSON output helpers such as `*_to_json` are DTO serialization, not
  domain types. Keep them separate from schema conversions.

## Protocol and contract guardrails

- Reuse source-of-truth terms. Do not invent new names for public surfaces
  unless the rename is explicit.
- Keep public contracts at the right layer. Host internals must not leak into
  SDK/WIT naming or semantics.
- Do not reuse an existing abstraction if it changes the behavior model.
  Semantic fit matters more than code reuse.
- For protocol changes, write the exact interaction trace first and reject extra
  hops on hot paths.
- If something is conceptually one-way, stop before making it `await`-shaped. Fix
  the boundary instead of forcing it through request/response machinery.

## Design judgment

- Prefer the simpler end-to-end flow, not the purer local abstraction.
- Bias toward single-phase designs over multi-phase orchestration on the hot
  path.
- Keep data near the point where it is naturally produced and immediately
  consumed; split it into a second mechanism only when that separation buys
  something concrete.
- Do not defend abstraction boundaries that add complexity in the common case.
- Once the direct path exists, remove bridge-style dispatch layers and other
  transitional glue instead of letting them harden into architecture.

## Test quality

Prefer tests that protect behavior the project depends on: user-visible
workflows, domain invariants, security/auth boundaries, persistence and
wire-format compatibility, and easy-to-reintroduce regressions. Avoid tests that
only confirm serde/clap/std behavior, wrappers forwarding fields, builders
storing inputs, in-memory fakes round-tripping data, or brittle presentation
text. See [testing](/contributing/testing/) for the full bar.

:::tip
A useful self-check before merging: did the change stay small and local, does it
preserve the existing architecture and contracts, and can every new test name
the regression it would catch?
:::
