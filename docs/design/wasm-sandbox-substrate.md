# Wasm sandbox substrate

Status: accepted
Scope: `crates/omnifs-host` Wasmtime/WASI plumbing, provider runtime, embedded sandboxed tools, tool-specific WIT interfaces

## Context

omnifs has two Wasm execution shapes.

The first is the provider runtime. A provider is a long-lived component
loaded from provider configuration. It imports the omnifs host API,
exports lifecycle and handler functions, holds provider state, performs
callouts, uses auth and capability policy, participates in cache
invalidation, and may run timers.

The second is an embedded sandboxed tool. Archive extraction is the
first instance: the host ships a precompiled Wasm component, opens a
small set of WASI capabilities for one job, calls one typed function,
and then drops the store. Future tools may render, convert, inspect, or
materialize other host-owned data in the same pattern.

These two shapes share Wasmtime and WASI mechanics but not a high-level
runtime abstraction. Provider lifecycle semantics are not tool execution
semantics, and tool policy should not leak into provider authoring.

## Decisions

### D1. Share Wasm host primitives, not provider runtime

The reusable layer should be low-level and host-internal.

- Wasmtime engine and component construction helpers.
- Component loading and pre-instantiation helpers.
- WASI context construction.
- Preopen and capability helpers.
- Fuel, memory, and store-limit policy.
- Trap and sandbox error classification.

The provider runtime and sandboxed tools consume this substrate in
different ways. Providers remain long-lived components with omnifs
imports, callouts, auth, config, timers, and mutable host state.
Tools remain short-lived jobs with fresh stores, narrow preopens, no
network or environment by default, and a typed one-shot WIT call.

### D2. Keep provider WIT semantic

Providers should not receive a generic run tool API. Provider WIT should
expose product semantics such as `open-archive`, not host implementation
mechanics such as component names, preopen maps, or fuel budgets.

This keeps provider code stable if the host changes how a tool is
implemented. It also keeps policy ownership in the host: providers ask for
semantic operations, and the host decides which embedded tool, limits,
cache keys, and publication behavior apply.

### D3. Use WIT for Wasm component boundaries

WIT is the right contract language for boundaries between the host and
Wasm components, and for boundaries between composed Wasm components.
Provider WIT and tool WIT both fit this model.

This should not be described as a general contract between
containers. If omnifs later splits work across OS processes or Docker
containers, that boundary should use an explicit IPC protocol such as a
Unix socket, HTTP, gRPC, or another transport-specific protocol.
WIT may still describe the component shape on either side, but it is
not itself a process transport.

### D4. Tool crates stay in `omnifs-tool-*` and stay statically embedded for now

The initial tool model keeps embedded components under host control.
Archive extraction follows this model: the host build embeds the reviewed
component artifact and the host chooses when to invoke it.

Future tool crates should use the `omnifs-tool-*` package naming pattern.
The current archive implementation is `omnifs-tool-archive`.
A dynamic tool registry is premature. It becomes useful only when tools
are independently versioned, configured, loaded outside the host binary,
or delegated to another trust domain. Until then, static embedding keeps
review, validation, and deployment simple.

### D5. Materializing tools publish through tree refs

Tools that produce directory trees should follow the same publication
shape as archive extraction.

1. Resolve or create a host-owned input blob.
2. Compute a semantic cache key for the requested view.
3. Run the tool into a temporary output directory.
4. Publish the output directory into the cache with a final rename.
5. Register the published path in `TreeRefs`.
6. Return a `tree-ref` to the provider.

The semantic cache key belongs to the tool adapter, not the generic
substrate. For archives, the key is `(cache_key, format, strip_prefix)`
where `cache_key` is the provider cache key used for blob identity.
Archive identity must stay stable across process restarts.

A second process must be able to resolve the same view key to the same
disk content without depending on previous runtime IDs.

### D6. Tool adapters own domain errors

The shared substrate should report sandbox failures such as load errors,
link errors, preopen errors, traps, fuel exhaustion, memory limit trips,
and WASI setup failures.

Tool adapters own domain failures. Archive extraction owns malformed
archives, unsafe paths, unsupported entry kinds, per-file limits, and
total output limits. Keeping those errors with the adapter prevents the
substrate from becoming a weak union of every future tool's result shape.

### D7. File publication helpers must be honest about durability

Shared publication helpers should name the actual guarantee. A helper that
writes a temporary file and renames it into place is an atomic visibility
helper on a single filesystem. It is not necessarily crash-durable
unless it also fsyncs the file and parent directory.

Use names like `replace_file_via_temp_rename` and
`publish_dir_by_rename` rather than `atomic_write` unless the helper
actually implements stronger durability.

## Proposed host layout

The final shape should separate common Wasm host mechanics from the
runtimes that use them.

```text
crates/omnifs-host/src/runtime/
  wasm/
    mod.rs
    engine.rs       # Wasmtime Config, Engine, Component helpers
    limits.rs       # fuel, memory, and store-limit policy
    wasi.rs         # WasiCtx and preopen construction helpers
    error.rs        # shared sandbox/load/link/trap errors

  provider/
    mod.rs          # long-lived provider runtime

  sandbox/
    mod.rs
    preopen.rs      # blob staging and narrow preopen helpers
    publish.rs      # temp-dir/temp-file publication helpers
    tree_cache.rs   # keyed materialization into TreeRefs

  tools/
    archive.rs      # archive WIT adapter, view key, limits, errors
```

The exact file names can move with the implementation, but the direction
should hold: `runtime::wasm` contains reusable component host primitives;
the provider runtime owns provider lifecycle; `runtime::tools` owns semantic
tool adapters; `runtime::sandbox` owns one-shot materialization support.

## Archive adapter target shape

Archive extraction reads like a semantic adapter using the substrate.

```rust
let key = self.view_key_for(request);
let tree = self.tree_materializer.materialize(key, |tmp_dir| {
    let blob_path = self.blob_cache.blob_path(&record.cache_key);
    self.archive_component.extract(ArchiveExtractRequest {
        blob_path: blob_path.as_path(),
        output_dir: tmp_dir,
        format,
        strip_prefix,
    })
})?;
```

The archive adapter owns:

- Translating provider `archive-format` to the archive extractor WIT.
- Constructing the archive view key.
- Mapping archive-specific errors to `CalloutResponse`.
- Choosing archive extraction limits.
- Deriving cache identity from provider cache key, format, and strip prefix.

The substrate owns:

- Building the store.
- Applying fuel and memory limits.
- Constructing WASI preopens.
- Classifying traps and sandbox setup failures.
- Publishing a completed tree without exposing partial output.

## Non-goals

- A provider-visible generic tool runner.
- A dynamic provider registry for sandbox tools.
- An IPC RPC layer between OS containers.
- A single trait that erases all tool input and output types.
- Moving provider lifecycle, callout handling, or cache invalidation
  into the sandboxed-tool path.

## Migration path

The archive hardening work extracts only mechanics that are clearly
tool-neutral: WASI preopen setup, blob staging, Wasmtime limits,
component-engine construction, temp publication, and tree materialization.
`ArchiveExecutor` remains the provider-facing semantic operation handler
for `open-archive`, while
`runtime::tools::archive::ArchiveExtractorComponent` owns the archive
extractor WIT adapter. `ArchiveExecutor` owns archive cache identity
policy because the key is part of the provider-facing operation.

Future embedded tool adapters should follow the same split. They will keep
execution in the shared substrate and own only their own domain semantics,
error model, and view keys.

Do not introduce a dynamic registry or a generic provider tool API until a
second real tool forces that contract.
