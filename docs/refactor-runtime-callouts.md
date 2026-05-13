# Runtime callout pipeline cleanup plan

## Goal

Make the runtime callout/op execution path in `crates/host/src/runtime/`
clean, maintainable, and performant. Remove tracing/logic entanglement,
cut duplicated translation layers, collapse parallel type hierarchies,
and split the oversized `runtime/mod.rs` (~1840 lines) into focused
modules.

## Constraints

- Unreleased work — no compatibility shims, no deprecation paths, no
  backwards-compat aliases.
- One commit per numbered step. Run `just check` after every commit.
  Batch steps only if the user explicitly approves a tiny mechanical
  grouping; phases are firm boundaries.
- No behavior change unless flagged explicitly. Phases 2.6 and 2.7 are
  the tracing exceptions: 2.6 restructures the tracing surface
  (span-based context, renamed events) and 2.7 adds the unsupported
  callout warning/inner span. Every other phase preserves tracing output
  byte-for-byte.
- Stop and ask the user before steps marked **CONFIRM**.

## Pre-flight

- [ ] Read this whole plan top-to-bottom before starting.
- [ ] Use `EnterWorktree` to isolate work.
- [ ] Establish baseline: `just check` (must pass before any change).
- [ ] Capture baseline test counts per workspace for the wrap-up summary.

## Reference: where the callout machinery lives today

Read these before touching anything:

- `crates/host/src/runtime/mod.rs` — `ProviderRuntime`, `Op`, `Callouts`
  struct, `Validator`, `apply_effects`, all WIT↔cache `From` impls,
  `LogUrl`/`LogHeaders` redaction.
- `crates/host/src/runtime/executor.rs` — `HttpExecutor`,
  `CalloutResponse`, `ErrorKind`.
- `crates/host/src/runtime/blob.rs` — `BlobExecutor`, blob fetch/read.
- `crates/host/src/runtime/git.rs` — `GitExecutor`.
- `crates/host/src/runtime/archive.rs` — `ArchiveExecutor`.
- `crates/host/src/runtime/browse_pipeline.rs` — five op wrappers,
  `coalesced`, `call_provider_op`.
- `crates/omnifs-sdk/src/cx.rs` — `join_all` ordering invariant
  (positional FIFO), do not break.

---

## Phase 1: Mechanical low-risk cleanups (no callout-pipeline changes)

These are independent and safe. Do them first to shrink the surface
before the bigger moves.

### 1.1 Delete dead `RuntimeError::UnexpectedResponse`

**Files**: `crates/host/src/runtime/mod.rs`.

**Approach**: The variant is defined but never constructed. Delete it.

```rust
// DELETE:
#[error("unexpected response type")]
UnexpectedResponse,
```

**Verify**: `just check`. No build errors.

**Done when**: variant gone, build clean.

---

### 1.2 Stop hardcoding `id = 0` for `Initialize`

**Files**: `crates/host/src/runtime/mod.rs` (`ProviderRuntime::initialize`).

**Approach**: Today `initialize()` calls `op.execute(self, 0)`; every
other op uses `correlations.allocate()`. Always allocate. Cost is one
atomic increment.

```rust
pub fn initialize(&self) -> Result<wit_types::OpResult> {
    let id = self.correlations.allocate();
    let op = Op::Initialize;
    match op.execute(self, id)? {
        wit_types::ProviderStep::Returned(ret) => self.finish_provider_return(&op, ret),
        wit_types::ProviderStep::Suspended(_) => Err(RuntimeError::ProviderProtocol(
            "initialize suspended with callouts".to_string(),
        )),
    }
}
```

**Verify**: `just check`.

**Later note**: Phase 5.0 turns initialize back into a direct lifecycle
call on `ProviderInstance`, because the WIT lifecycle method returns an
`OpResult` directly and cannot suspend. Until that boundary exists,
this step removes the hardcoded `0` from the current `Op::execute`
path.

---

### 1.3 Collapse cache `_with_aux` variants into `Option<&str>`

**Files**: `crates/host/src/runtime/mod.rs` plus call sites
(grep `cache_get\|cache_get_with_aux\|cache_put\|cache_put_with_aux`).

**Approach**: Replace four methods with two that take `aux: Option<&str>`.

```rust
pub fn cache_get(&self, path: &str, kind: RecordKind, aux: Option<&str>) -> Option<CacheRecord> {
    self.l2.as_ref()?.get(&Key::with_aux(path, kind, aux)).ok().flatten()
}

pub fn cache_put(&self, path: &str, kind: RecordKind, aux: Option<&str>, record: &CacheRecord) {
    if let Some(ref l2) = self.l2
        && let Err(e) = l2.put(&Key::with_aux(path, kind, aux), record)
    {
        debug!(path, error = %e, "L2 cache put failed");
    }
}
```

Update every caller. Existing `cache_get(path, kind)` becomes
`cache_get(path, kind, None)`.

**Verify**: `just check`. Run `cargo test -p omnifs-host`.

---

### 1.4 Move `CapabilityGrants`/`BlobLimits` config parsing onto the type

**Files**:
- `crates/host/src/runtime/capability.rs` (add `from_config`).
- `crates/host/src/runtime/blob.rs` (add `from_config`).
- `crates/host/src/runtime/mod.rs` (delete `build_grants` and
  `blob_limits_from_config`, update `ProviderRuntime::new` call sites).

**Approach**:

```rust
// crates/host/src/runtime/capability.rs
impl CapabilityGrants {
    pub fn from_config(config: &crate::config::InstanceConfig, needs_git: bool) -> Self {
        let caps = config.capabilities.as_ref();
        Self {
            domains: caps.and_then(|c| c.domains.clone()).unwrap_or_default(),
            git_repos: caps.and_then(|c| c.git_repos.clone()).unwrap_or_default(),
            max_memory_mb: caps.and_then(|c| c.max_memory_mb).unwrap_or(64),
            needs_git,
        }
    }
}

// crates/host/src/runtime/blob.rs
impl BlobLimits {
    pub fn from_config(config: &crate::config::InstanceConfig) -> Self {
        let defaults = Self::default();
        let caps = config.capabilities.as_ref();
        Self {
            max_fetch_blob_bytes: caps.and_then(|c| c.max_fetch_blob_bytes).unwrap_or(defaults.max_fetch_blob_bytes),
            max_read_blob_bytes:  caps.and_then(|c| c.max_read_blob_bytes).unwrap_or(defaults.max_read_blob_bytes),
        }
    }
}
```

`ProviderRuntime::new` call sites:

```rust
let grants = CapabilityGrants::from_config(config, provider_caps.needs_git);
// ...
let blob_limits = BlobLimits::from_config(config);
```

Delete the old free functions.

**Verify**: `just check`.

---

### 1.5 Extract activity-touch resolver onto `DeclaredHandler`, drop the tuple

**Files**:
- `crates/host/src/runtime/activity.rs` (define `ActivePathTouch`,
  update `ActivityTable::touch` signature).
- `crates/host/src/runtime/manifest.rs` (add resolver method).
- `crates/host/src/runtime/browse_pipeline.rs` (slim down
  `touch_activity_for_relative_path`).

**Approach**: The "find most-specific declared handler matching an
absolute path" logic in `touch_activity_for_relative_path` has nothing
to do with the activity table. Move it to where it belongs.

While moving, **drop the `(String, String, String)` tuple** that
`ActivityTable::touch` takes today. Three same-typed strings whose
meaning is documented only by call-site convention is a footgun
(swap-the-fields bugs). Define a typed record instead.

```rust
// crates/host/src/runtime/activity.rs
#[derive(Debug, Clone)]
pub struct ActivePathTouch {
    pub mount_id: String,
    pub mount_name: String,
    pub path: String,
}

impl ActivityTable {
    pub fn touch<I>(&mut self, touched: I)
    where
        I: IntoIterator<Item = ActivePathTouch>,
    {
        let now = Instant::now();
        for ActivePathTouch { mount_id, mount_name, path } in touched {
            self.entries
                .entry(mount_id)
                .or_insert_with(|| ActiveMountEntry { mount_name, paths: HashMap::new() })
                .paths
                .insert(path, now);
        }
    }
}
```

```rust
// crates/host/src/runtime/manifest.rs
impl DeclaredHandler {
    pub fn resolve_touched(handlers: &[DeclaredHandler], absolute: &str)
        -> Vec<crate::runtime::activity::ActivePathTouch>
    {
        let mut best_by_depth = std::collections::BTreeMap::new();
        for mount in handlers {
            let Some(concrete_path) = mount.concrete_path_for(absolute) else { continue };
            match best_by_depth.entry(mount.pattern_len()) {
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert((mount, concrete_path));
                },
                std::collections::btree_map::Entry::Occupied(mut slot) => {
                    let current = slot.get().0;
                    if mount.specificity().iter().cmp(current.specificity().iter()).is_gt() {
                        slot.insert((mount, concrete_path));
                    }
                },
            }
        }
        best_by_depth
            .into_values()
            .map(|(mount, concrete_path)| crate::runtime::activity::ActivePathTouch {
                mount_id: mount.mount_id.clone(),
                mount_name: mount.mount_name.clone(),
                path: concrete_path,
            })
            .collect()
    }
}
```

Caller becomes:

```rust
fn touch_activity_for_relative_path(&self, path: &str) {
    let absolute = super::absolute_mount_path(path);
    let touched = DeclaredHandler::resolve_touched(&self.declared_handlers, &absolute);
    if !touched.is_empty() {
        self.activity_table.lock().touch(touched);
    }
}
```

**Verify**: `just check`. `ActivityTable::touch` has exactly one
caller (the resolver above), so the signature change is contained.

---

### 1.6 Rename `CorrelationTracker` to `OperationIds`

**Files**:
- `crates/host/src/runtime/correlation.rs` → rename to
  `crates/host/src/runtime/operation_ids.rs`.
- `crates/host/src/runtime/mod.rs` (`mod correlation;` → `mod operation_ids;`,
  field rename `correlations: CorrelationTracker` →
  `operation_ids: OperationIds`).
- All call sites (`self.correlations.allocate()` →
  `self.operation_ids.allocate()`).

**Approach**: The current type is `pub struct CorrelationTracker { next: AtomicU64 }`
with one method `allocate()`. It does not track anything; it allocates
ids. Rename to match what it does. If a future change ever adds
pending-operation tracking (a map of id → operation), the name can
expand back; for now, an honest name beats an aspirational one.

```rust
// crates/host/src/runtime/operation_ids.rs
pub struct OperationIds {
    next: std::sync::atomic::AtomicU64,
}

impl OperationIds {
    pub const fn new() -> Self {
        Self { next: std::sync::atomic::AtomicU64::new(1) }
    }

    pub fn allocate(&self) -> u64 {
        self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }
}

impl Default for OperationIds {
    fn default() -> Self { Self::new() }
}
```

**Verify**: `just check`. Pure rename — no behavior change.

---

## Phase 2: Callout pipeline rewrite

This is the heart of the cleanup. Steps 2.1–2.9 are interrelated and
should land as a small chain of commits. Read all of Phase 2 before
starting.

### 2.1 Push `spawn_blocking` into `ArchiveExecutor::open_archive`

**Files**:
- `crates/host/src/runtime/archive.rs` (make `open_archive` async,
  internalize `tokio::task::spawn_blocking`).
- `crates/host/src/runtime/mod.rs` (callout dispatch arm shrinks).

**Approach**: Today the runtime knows that archive extraction is
CPU-heavy and spawns a blocking task at the dispatch site. That's a
leak. Push it down.

```rust
// crates/host/src/runtime/archive.rs
impl ArchiveExecutor {
    pub async fn open_archive(
        self: &Arc<Self>,
        blob: u64,
        format: ArchiveFormat,
        strip_prefix: Option<&str>,
    ) -> CalloutResponse {
        let this = Arc::clone(self);
        let strip = strip_prefix.map(|s| s.to_string());
        tokio::task::spawn_blocking(move || this.open_archive_blocking(blob, format, strip.as_deref()))
            .await
            .unwrap_or_else(|join_err| CalloutResponse::Error {
                kind: ErrorKind::Internal,
                message: format!("extract task join: {join_err}"),
                retryable: false,
            })
    }

    fn open_archive_blocking(
        &self,
        blob: u64,
        format: ArchiveFormat,
        strip_prefix: Option<&str>,
    ) -> CalloutResponse {
        // ... current synchronous body of open_archive ...
    }
}
```

Update tests that called `open_archive` synchronously to `.await` it
(some of them may need to wrap in a tokio runtime). Because the public
async method takes `self: &Arc<Self>` so it can move the executor into
`spawn_blocking`, tests that currently hold a bare `ArchiveExecutor`
should wrap it in `Arc::new(...)` before calling the public method.

Dispatch arm in `mod.rs` becomes:

```rust
wit_types::Callout::OpenArchive(req) => {
    let format = ArchiveFormat::from(req.format);
    self.runtime.archive.open_archive(req.blob, format, req.strip_prefix.as_deref()).await
},
```

**Verify**: `just check`. Archive tests must still pass.

---

### 2.2 Push HTTP header conversion into executors; make `build_header_map` borrow

**Files**:
- `crates/host/src/runtime/http_headers.rs` (signature change to
  borrow).
- `crates/host/src/runtime/executor.rs` (`HttpExecutor::execute_fetch`
  becomes `HttpExecutor::fetch`).
- `crates/host/src/runtime/blob.rs` (`BlobExecutor::fetch_blob`
  becomes `BlobExecutor::fetch`).
- `crates/host/src/runtime/mod.rs` (drop request-side `header_pairs`
  calls in callout dispatch/start logs).

**Approach**: Move header construction into the executors AND fix the
upstream signature so we don't just relocate the cloning. Today's
`build_header_map(&[(String, String)], &[(String, String)])` forces a
`Vec<(String, String)>` to exist before the call; passing borrowed
WIT slices through eliminates that.

```rust
// crates/host/src/runtime/http_headers.rs
pub(crate) fn build_header_map<'a, A, R>(
    auth_headers: A,
    request_headers: R,
) -> Result<HeaderMap, String>
where
    A: IntoIterator<Item = (&'a str, &'a str)>,
    R: IntoIterator<Item = (&'a str, &'a str)>,
{
    let mut map = HeaderMap::new();
    append(&mut map, auth_headers, "auth")?;
    append(&mut map, request_headers, "request")?;
    Ok(map)
}

fn append<'a, I>(map: &mut HeaderMap, headers: I, source: &str) -> Result<(), String>
where I: IntoIterator<Item = (&'a str, &'a str)>
{
    for (name, value) in headers {
        let n = HeaderName::from_str(name).map_err(/* ... */)?;
        let v = HeaderValue::from_str(value).map_err(/* ... */)?;
        map.append(n, v);
    }
    Ok(())
}
```

Caller passes WIT headers as borrowed pairs without intermediate
`Vec`. Critically, 2.2 lands BEFORE 2.3, so `HttpExecutor::fetch`
still returns `CalloutResponse` here — the `?` operator does not
work against `Result<HeaderMap, String>`, so use explicit `match`/
early-return until 2.3 flips the return type to `CalloutResult`:

```rust
// crates/host/src/runtime/executor.rs (still returns CalloutResponse at this point)
impl HttpExecutor {
    pub async fn fetch(&self, req: &wit_types::HttpRequest) -> CalloutResponse {
        if let Err(e) = self.capability.check_url(&req.url) {
            return CalloutResponse::Error { kind: ErrorKind::Denied, message: e.to_string(), retryable: false };
        }
        let auth_headers = self.auth.headers_for_url(&req.url);
        // ... auth/method checks identical to today, returning CalloutResponse::Error ...
        let header_map = match build_header_map(
            auth_headers.iter().map(|(n, v)| (n.as_str(), v.as_str())),
            req.headers.iter().map(|h| (h.name.as_str(), h.value.as_str())),
        ) {
            Ok(map) => map,
            Err(message) => return CalloutResponse::Error { kind: ErrorKind::Internal, message, retryable: false },
        };
        // ... rest of existing body, no Vec<(String, String)> allocated.
    }
}
```

Same shape for `BlobExecutor::fetch`.

This step intentionally renames the request-shaped public methods while
moving header conversion:

- `HttpExecutor::execute_fetch(method, url, headers, body)` →
  `HttpExecutor::fetch(req: &wit_types::HttpRequest)`.
- `BlobExecutor::fetch_blob(method, url, headers, body, cache_key)` →
  `BlobExecutor::fetch(req: &wit_types::BlobFetchRequest)`.

`BlobExecutor::read_blob` keeps its scalar signature until 2.6, where
the tracing annotations consolidate it to `read(req)`.

Also update the start-log sites in `mod.rs` to log request headers
directly from the WIT request instead of cloning through `header_pairs`:

```rust
request_headers = %WitHeaders(&req.headers),
```

After this, capability/auth checks happen before any header allocation
and the dispatch layer no longer clones request headers for logging or
executor calls.

**Also in this commit: introduce the `WitHeaders` redaction wrapper.**
2.3 needs to log `&[wit_types::Header]` directly (the WIT response
headers carried by `BlobFetched`/`HttpResponse` are not tuple-shaped),
and `header_pairs` is on the way out. Land the helper now so 2.3 can
use it:

```rust
// crates/host/src/runtime/mod.rs (or log_redaction.rs after 5.1)
pub(crate) struct WitHeaders<'a>(pub(crate) &'a [wit_types::Header]);

impl fmt::Display for WitHeaders<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, h) in self.0.iter().enumerate() {
            if i > 0 { f.write_char(',')?; }
            write!(f, "{}=", h.name)?;
            if is_sensitive_header(&h.name) {
                f.write_str("<redacted>")?;
            } else {
                write_truncated_for_log(f, &h.value, 256)?;
            }
        }
        Ok(())
    }
}
```

Delete `header_pairs` in this step. The central response logger still
uses `LogHeaders` for executor-owned `(String, String)` response header
vectors until 2.3 converts responses to WIT, and then uses `WitHeaders`
for WIT response headers.

**Verify**: `just check`. HTTP and blob fetch integration tests pass.
Confirm the intermediate `Vec<(String, String)>` is gone from both
executor methods and callout dispatch (`grep -n 'header_pairs\|Vec<(String, String)>' crates/host/src/runtime/{executor,blob,mod}.rs`
should not show request-side conversion; tuple vectors may still appear
where response headers are stored before 2.3).

---

### 2.3 Eliminate `CalloutResponse`; executors return `wit_types::CalloutResult`

**Files**:
- `crates/host/src/runtime/executor.rs` (delete `CalloutResponse`).
- `crates/host/src/runtime/blob.rs`, `git.rs`, `archive.rs` (return
  `wit_types::CalloutResult` directly).
- `crates/host/src/runtime/mod.rs` (delete `callout_response_to_wit`,
  `git_response_to_wit`, `blob_response_to_wit`, `archive_response_to_wit`,
  `blob_read_to_wit`, `unsupported`, `unexpected`,
  rework `log_callout_response` to take the original `&Callout`).

**Approach**: `CalloutResponse` mirrors `CalloutResult` 1:1 for no
benefit. Each executor owns construction of its WIT result variant.

**Layering rule (load-bearing)**: only the executor's *public entry
method* constructs `wit_types::CalloutResult`. Internal helpers
(`stream_response_body`, `read_range`, archive walk, etc.) keep
returning typed `Result<T, BlobError>` / `Result<T, ArchiveError>`.
Convert at the boundary via a single `From<BlobError> for wit_types::CalloutResult`
(or a one-line match in the public method). `CalloutResult`
construction must not leak into every helper.

```rust
// crates/host/src/runtime/executor.rs — only the ErrorKind enum remains.
// Delete CalloutResponse entirely.

// crates/host/src/runtime/git.rs (example — public entry method)
pub fn open_repo(&self, req: &wit_types::GitOpenRequest) -> wit_types::CalloutResult {
    match self.open_repo_inner(req) {                         // typed Result internally
        Ok(id) => wit_types::CalloutResult::GitRepoOpened(wit_types::GitRepoInfo { repo: id, tree: id }),
        Err(e) => e.into(),                                   // single conversion point
    }
}

fn open_repo_inner(&self, req: &wit_types::GitOpenRequest) -> Result<u64, GitError> {
    self.capability.check_git_url(&req.clone_url)?;
    let cache_path = self
        .cloner
        .clone_if_needed(&req.cache_key, &req.clone_url)
        .map_err(|error| {
            tracing::warn!(
                cache_key = %req.cache_key,
                clone_url = %req.clone_url,
                error = %error,
                "clone failed"
            );
            GitError::from(error)
        })?;
    Ok(self.trees.register(cache_path))
}

#[derive(Debug, thiserror::Error)]
enum GitError {
    #[error("denied: {0}")]
    Denied(String),
    #[error("clone failed: {0}")]
    Clone(String),
}

impl From<crate::runtime::capability::CapabilityError> for GitError {
    fn from(error: crate::runtime::capability::CapabilityError) -> Self {
        Self::Denied(error.to_string())
    }
}

impl From<crate::runtime::cloner::CloneError> for GitError {
    fn from(error: crate::runtime::cloner::CloneError) -> Self {
        Self::Clone(error.to_string())
    }
}

impl From<GitError> for wit_types::CalloutResult {
    fn from(error: GitError) -> Self {
        match error {
            GitError::Denied(msg) => callout_denied(msg),
            // Preserve today's behavior: clone failures map to Network +
            // retryable=true (see runtime/git.rs:45 in current code). This
            // is intentional — transient network blips during git clone
            // are common and the runtime trusts the WIT-level retry hint.
            // Keep the provider-visible message exactly as clone_if_needed
            // produced it; the contextual warn above carries cache_key/url.
            GitError::Clone(msg)  => callout_network(msg),
        }
    }
}
```

Same shape for `BlobExecutor` (already has a useful `BlobError` —
preserve it; add `From<BlobError> for wit_types::CalloutResult`) and
`ArchiveExecutor` (preserve `ArchiveError`).

Keep success-construction helpers module-local when the mapping is more
than a one-line enum arm. For blob fetch, for example, replace the old
central `blob_response_to_wit` branch with a private helper in
`blob.rs`; do not recreate a shared response-conversion layer:

```rust
fn blob_fetched_to_wit(record: &BlobRecord) -> wit_types::CalloutResult {
    wit_types::CalloutResult::BlobFetched(wit_types::BlobFetched {
        blob: record.id,
        size: record.size,
        content_type: record.content_type.clone(),
        etag: record.etag.clone(),
        status: record.status,
        response_headers: record
            .response_headers
            .iter()
            .map(|(name, value)| wit_types::Header { name: name.clone(), value: value.clone() })
            .collect(),
    })
}
```

Use the constructors added in 2.4 for the boundary conversion. Until
2.4 lands, inline the `wit_types::CalloutResult::CalloutError(...)`
literal in the `From` impl.

The `From<ErrorKind> for wit_types::ErrorKind` impl in `mod.rs` stays;
each executor's `From<XxxError>` impl uses it.

**Logging-shape preservation requirement.** Today's response log for
blob.fetch emits `cache_key` sourced from the internal `BlobRecord`
(see `mod.rs::log_callout_response` — `BlobFetched` arm). The WIT
`BlobFetched` variant has no `cache_key` field. To keep log shape
stable, change `log_callout_response` to also take `&wit_types::Callout`
so blob.fetch can source `cache_key` from `callout::FetchBlob.cache_key`:

```rust
fn log_callout_response(
    operation_id: u64,
    callout_index: usize,
    callout_kind: &str,
    callout: &wit_types::Callout,
    elapsed: std::time::Duration,
    response: &wit_types::CalloutResult,
) {
    let elapsed_us = elapsed.as_micros();
    match response {
        wit_types::CalloutResult::HttpResponse(r) => { /* status, headers, body bytes */ },
        wit_types::CalloutResult::GitRepoOpened(r) => { /* tree_ref = r.tree */ },
        wit_types::CalloutResult::ArchiveOpened(r) => { /* tree_ref = r.tree */ },
        wit_types::CalloutResult::BlobFetched(r) => {
            let cache_key = match callout {
                wit_types::Callout::FetchBlob(req) => req.cache_key.as_str(),
                _ => "",
            };
            info!(
                target: "omnifs_callout",
                operation_id, callout_index, callout_kind,
                blob = r.blob,
                cache_key,
                status = r.status,
                response_headers = %WitHeaders(&r.response_headers),     // WitHeaders introduced in 2.2
                response_body_bytes = r.size,
                elapsed_us,
                "callout response",
            );
        },
        wit_types::CalloutResult::BlobRead(bytes)  => { /* response_body_bytes = bytes.len() */ },
        wit_types::CalloutResult::CalloutError(e)  => { /* warn! */ },
        _ => tracing::warn!(target: "omnifs_callout", operation_id, callout_index, callout_kind, "unhandled callout result variant"),
    }
}
```

The dispatch wrapper passes both `callout` and the result through:

```rust
log_callout_response(operation_id, index, kind.as_str(), callout, started.elapsed(), &result);
```

**Verify**: `just check`. Diff a `tail -f /tmp/omnifs.log` capture
against a baseline run that exercises every callout kind (FUSE smoke
harness should suffice). Confirm `cache_key` appears on blob.fetch
response lines and every other field name matches today's output.

**Note**: This step is the biggest single deletion (~150 lines). Land
it as one commit even though the diff is wide — the change is uniformly
mechanical across the four executor files.

---

### 2.4 Add `wit_types::CalloutResult` constructor helpers

**Files**: new `crates/host/src/runtime/callouts.rs` (created in 5.5)
or, until then, into `mod.rs` as private helpers. Land them locally
where most error returns live, then move with the file split in 5.5.

**Approach**:

```rust
fn callout_error(kind: wit_types::ErrorKind, message: impl Into<String>, retryable: bool) -> wit_types::CalloutResult {
    wit_types::CalloutResult::CalloutError(wit_types::CalloutError {
        kind, message: message.into(), retryable,
    })
}

fn callout_internal(message: impl Into<String>)  -> wit_types::CalloutResult { callout_error(wit_types::ErrorKind::Internal, message, false) }
fn callout_denied(message: impl Into<String>)    -> wit_types::CalloutResult { callout_error(wit_types::ErrorKind::Denied, message, false) }
fn callout_not_found(message: impl Into<String>) -> wit_types::CalloutResult { callout_error(wit_types::ErrorKind::NotFound, message, false) }
fn callout_too_large(message: impl Into<String>) -> wit_types::CalloutResult { callout_error(wit_types::ErrorKind::TooLarge, message, false) }
fn callout_invalid(message: impl Into<String>)   -> wit_types::CalloutResult { callout_error(wit_types::ErrorKind::InvalidInput, message, false) }
fn callout_network(message: impl Into<String>)   -> wit_types::CalloutResult { callout_error(wit_types::ErrorKind::Network, message, true) }
```

Replace the ~30 verbose struct-literal constructions across the executor
modules. Note `network` defaults retryable=true; others default false.

The `_executor` modules need to import these. If they're private to
`mod.rs`, expose them as `pub(super)` until the 5.5 split moves them.

**Verify**: `just check`. Same callout error semantics as before.

---

### 2.5 Introduce typed `CalloutKind` enum

**Files**: `crates/host/src/runtime/mod.rs` (or `callouts.rs` after 5.5).

**Approach**: Replace the five `&'static str` literals
(`"http.fetch"`, `"git.open_repo"`, etc.) with a typed enum. Per Rust
forbidden patterns rule, stringly-typed APIs go away.

```rust
#[derive(Debug, Clone, Copy)]
pub(super) enum CalloutKind {
    HttpFetch,
    GitOpenRepo,
    BlobFetch,
    OpenArchive,
    ReadBlob,
    /// A WIT-defined callout the runtime knowingly does not implement
    /// yet (`stream-open`, `stream-recv`, `stream-close`, `ws-connect`,
    /// `ws-send`, `ws-recv`, `ws-close`). The provider gets a typed
    /// `callout-error{kind=internal, retryable=false}` back; the
    /// dispatch logs this as a known-unsupported variant, not as an
    /// unknown enum.
    Unsupported,
}

impl CalloutKind {
    pub(super) fn of(callout: &wit_types::Callout) -> Self {
        match callout {
            wit_types::Callout::Fetch(_)        => Self::HttpFetch,
            wit_types::Callout::GitOpenRepo(_)  => Self::GitOpenRepo,
            wit_types::Callout::FetchBlob(_)    => Self::BlobFetch,
            wit_types::Callout::OpenArchive(_)  => Self::OpenArchive,
            wit_types::Callout::ReadBlob(_)     => Self::ReadBlob,
            wit_types::Callout::StreamOpen(_)
            | wit_types::Callout::StreamRecv(_)
            | wit_types::Callout::StreamClose(_)
            | wit_types::Callout::WsConnect(_)
            | wit_types::Callout::WsSend(_)
            | wit_types::Callout::WsRecv(_)
            | wit_types::Callout::WsClose(_)    => Self::Unsupported,
        }
    }

    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::HttpFetch    => "http.fetch",
            Self::GitOpenRepo  => "git.open_repo",
            Self::BlobFetch    => "blob.fetch",
            Self::OpenArchive  => "archive.open",
            Self::ReadBlob     => "blob.read",
            Self::Unsupported  => "unsupported",
        }
    }
}
```

Note: the match is exhaustive (no `_` arm). If a future WIT change
adds a new `callout` variant, the match fails to compile and forces
the runtime author to make an explicit decision.

Pass `CalloutKind` through `log_callout_response` and the started-log
helper instead of `&str`.

**Verify**: `just check`. Tracing field values unchanged
(`callout_kind` strings identical).

---

### 2.6 Idiomatic span-based tracing (coordinated subscriber update)

**Files**:
- `crates/host/src/runtime/mod.rs` (or `callouts.rs` after 5.5).
- `crates/host/src/runtime/executor.rs`,
  `crates/host/src/runtime/blob.rs`,
  `crates/host/src/runtime/git.rs`,
  `crates/host/src/runtime/archive.rs` (each gets `#[instrument]`).
- `crates/cli/src/main.rs` (subscriber config — coordinated change).

**Approach**: today's style — five copies of
`info!(target: "omnifs_callout", operation_id = ..., callout_index = ..., callout_kind = ..., ...)`
threaded through every executor — is not idiomatic `tracing`. It
treats `tracing` as a glorified `println!` with key=value pairs and
re-passes the same context on every call.

Earlier drafts of this step centralized log emission in
`emit_started` / `emit_completed` matchers. That just relocated the
boilerplate. The actual fix is to **push observability into each
executor** so per-kind log fields live with the per-kind execution
code; the dispatch wrapper carries only cross-cutting context.

**Dispatch wrapper — the entire dispatch tracing surface**:

The snippet below shows the final post-2.8 method names. If you follow
the numeric order strictly, this code still lives inside the existing
`Callouts` helper in 2.6 and references executors through
`self.runtime`; do not add a bridge layer. Step 2.8 moves the same
wrapper onto `ProviderRuntime` when it deletes `Callouts`.

```rust
async fn dispatch_one(&self, op_id: u64, index: usize, callout: &wit_types::Callout)
    -> wit_types::CalloutResult
{
    self.run_callout(callout)
        .instrument(tracing::info_span!(
            target: "omnifs_callout",
            "callout",
            operation_id = op_id,
            callout_index = index,
            kind = CalloutKind::of(callout).as_str(),
        ))
        .await
}

async fn run_callout(&self, callout: &wit_types::Callout) -> wit_types::CalloutResult {
    match callout {
        wit_types::Callout::Fetch(req)        => self.http.fetch(req).await,
        wit_types::Callout::FetchBlob(req)    => self.blob.fetch(req).await,
        wit_types::Callout::GitOpenRepo(req)  => self.git.open_repo(req),
        wit_types::Callout::OpenArchive(req)  => self.archive.open(req).await,
        wit_types::Callout::ReadBlob(req)     => self.blob.read(req),
        _ => callout_internal("callout type not yet implemented"),
    }
}
```

That's it. No `emit_started`, no `emit_completed`, no `traced_callout`
helper, no central log dispatcher.

**Per-executor `#[instrument]` annotations** carry the kind-specific
fields. Each executor declares its own observability with the code
that produces it. **All semantically meaningful fields the current logs
emit must be preserved**, including `request_headers` and
`response_headers` for the HTTP-shaped paths. Empty placeholder fields
are called out explicitly below.

`WitHeaders` (introduced in 2.2) is the redaction `Display` wrapper
for `&[wit_types::Header]`. Both request- and response-side fields
inside `#[instrument]` blocks use it.

**Critical**: every field a span ever records must appear in
`fields(...)` (with `field::Empty` if late-bound). `Span::record` only
populates fields the span knows about; un-declared field names are
silently dropped. So error fields (`error.kind`, `error.message`,
`error.retryable`) get pre-declared on every executor span, alongside
the success-path fields.

Every span in this callout hierarchy keeps `target = "omnifs_callout"`.
That preserves existing `RUST_LOG=omnifs_callout=...` filters even
though the span names and fields change in 2.6.

```rust
// crates/host/src/runtime/executor.rs (or http_stack.rs after Phase 6)
impl HttpExecutor {
    #[tracing::instrument(target = "omnifs_callout", skip_all, fields(
        method = req.method.as_str(),
        url = %LogUrl(&req.url),
        request_headers = %WitHeaders(&req.headers),
        request_body_bytes = req.body.as_ref().map_or(0, Vec::len),
        status = tracing::field::Empty,
        response_headers = tracing::field::Empty,
        response_body_bytes = tracing::field::Empty,
        error.kind = tracing::field::Empty,
        error.message = tracing::field::Empty,
        error.retryable = tracing::field::Empty,
    ))]
    pub async fn fetch(&self, req: &wit_types::HttpRequest) -> wit_types::CalloutResult {
        let result = /* ... existing body ... */;
        record_outcome(&result);
        result
    }
}

// crates/host/src/runtime/blob.rs
impl BlobExecutor {
    #[tracing::instrument(target = "omnifs_callout", skip_all, fields(
        cache_key = %req.cache_key,
        method = req.method.as_str(),
        url = %LogUrl(&req.url),
        request_headers = %WitHeaders(&req.headers),
        request_body_bytes = req.body.as_ref().map_or(0, Vec::len),
        blob = tracing::field::Empty,
        status = tracing::field::Empty,
        response_headers = tracing::field::Empty,
        response_body_bytes = tracing::field::Empty,    // matches today's field name; not `size`
        error.kind = tracing::field::Empty,
        error.message = tracing::field::Empty,
        error.retryable = tracing::field::Empty,
    ))]
    pub async fn fetch(&self, req: &wit_types::BlobFetchRequest) -> wit_types::CalloutResult {
        let result = /* ... existing body ... */;
        record_outcome(&result);
        result
    }

    #[tracing::instrument(target = "omnifs_callout", skip_all, fields(
        blob = req.blob,
        offset = req.offset,
        len = ?req.len,
        response_body_bytes = tracing::field::Empty,
        error.kind = tracing::field::Empty,
        error.message = tracing::field::Empty,
        error.retryable = tracing::field::Empty,
    ))]
    pub fn read(&self, req: &wit_types::ReadBlobRequest) -> wit_types::CalloutResult {
        let result = /* ... existing body ... */;
        record_outcome(&result);
        result
    }
}

// crates/host/src/runtime/git.rs
impl GitExecutor {
    #[tracing::instrument(target = "omnifs_callout", skip_all, fields(
        url = %LogUrl(&req.clone_url),
        tree_ref = tracing::field::Empty,
        error.kind = tracing::field::Empty,
        error.message = tracing::field::Empty,
        error.retryable = tracing::field::Empty,
    ))]
    pub fn open_repo(&self, req: &wit_types::GitOpenRequest) -> wit_types::CalloutResult {
        let result = /* ... existing body ... */;
        record_outcome(&result);
        result
    }
}

// crates/host/src/runtime/archive.rs
impl ArchiveExecutor {
    #[tracing::instrument(target = "omnifs_callout", skip_all, fields(
        blob = req.blob,
        format = ?req.format,
        strip_prefix = req.strip_prefix.as_deref().unwrap_or(""),
        tree_ref = tracing::field::Empty,
        error.kind = tracing::field::Empty,
        error.message = tracing::field::Empty,
        error.retryable = tracing::field::Empty,
    ))]
    pub async fn open(self: &Arc<Self>, req: &wit_types::ArchiveOpenRequest) -> wit_types::CalloutResult {
        let result = /* ... existing body ... */;
        record_outcome(&result);
        result
    }
}
```

Shared `record_outcome` lives in `callouts.rs` (after 5.5) or `mod.rs`
until then. It records the right fields on `Span::current()` based on
the result variant:

```rust
pub(super) fn record_outcome(result: &wit_types::CalloutResult) {
    let span = tracing::Span::current();
    match result {
        wit_types::CalloutResult::HttpResponse(r) => {
            span.record("status", r.status);
            span.record("response_headers", tracing::field::display(WitHeaders(&r.headers)));
            span.record("response_body_bytes", r.body.len());
        },
        wit_types::CalloutResult::BlobFetched(r) => {
            span.record("blob", r.blob);
            span.record("status", r.status);
            span.record("response_headers", tracing::field::display(WitHeaders(&r.response_headers)));
            span.record("response_body_bytes", r.size);
        },
        wit_types::CalloutResult::BlobRead(bytes) => {
            span.record("response_body_bytes", bytes.len());
        },
        wit_types::CalloutResult::GitRepoOpened(r) => { span.record("tree_ref", r.tree); },
        wit_types::CalloutResult::ArchiveOpened(r) => { span.record("tree_ref", r.tree); },
        wit_types::CalloutResult::CalloutError(e) => {
            span.record("error.kind", tracing::field::debug(&e.kind));
            span.record("error.message", e.message.as_str());
            span.record("error.retryable", e.retryable);
        },
        _ => {},
    }
}
```

Each executor calls `record_outcome(&result)` once before returning.
One central match over the result variants, but it lives in the
tracing layer (not in business logic) and only records — it does not
construct results.

**Load-bearing implementation rule**: instrumented public callout
methods must not return early. Build a `result`, call
`record_outcome(&result)`, then return it. Internal helpers may use
`?` and early returns freely; the public boundary is where tracing is
closed over the final `CalloutResult`.

The `#[instrument]` macro creates a child span per call. Request-side
fields are evaluated at span construction. Response-side fields
(`status`, `response_headers`, `response_body_bytes`, `tree_ref`)
are pre-declared as `Empty` and recorded via `Span::current().record(...)`
on the success path so they appear by the time the span closes.

**Field mapping check**: every semantically meaningful field today's
logs emit, where it lives now:

| Today (event field) | After 2.6 (where recorded) |
|---|---|
| `operation_id` | outer `callout` span |
| `callout_index` | outer `callout` span |
| `callout_kind` | outer `callout` span as `kind` |
| `method` | inner executor span (HTTP / blob fetch) |
| `url` | inner executor span (HTTP / blob fetch / git) |
| `request_headers` | inner executor span (HTTP / blob fetch) |
| `request_body_bytes` | inner executor span (HTTP / blob fetch) |
| `cache_key` | inner blob fetch span |
| `blob` | inner blob fetch span (recorded on success), blob read span, archive open span |
| `format` | inner archive open span |
| `strip_prefix` | inner archive open span |
| `offset` / `len` | inner blob read span |
| `status` | inner span, recorded on success |
| `response_headers` | inner span, recorded on success |
| `response_body_bytes` | inner span, recorded on success (HTTP / blob fetch maps from `r.size`; blob read from `bytes.len()`) |
| `tree_ref` | inner span, recorded on success (git / archive) |
| `elapsed_us` | gone; replaced by framework span timing on `FmtSpan::CLOSE` |
| `error.kind` / `error.message` / `error.retryable` | inner span on error path (predeclared as `Empty`, recorded by `record_outcome`) |

The current `git.open_repo` start event also emits shape-padding fields
`method=""`, `request_headers=""`, and `request_body_bytes=0`. Those
empty placeholders are intentionally removed in the 2.6 tracing
format change; they carried no request data.

**Subscriber update** (lands in the same commit):

```rust
// crates/cli/src/main.rs
use tracing_subscriber::{EnvFilter, fmt::format::FmtSpan};

tracing_subscriber::fmt()
    .with_env_filter(
        EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("info")),
    )
    .with_target(false)
    // NEW = span constructed (boundary marker for "started").
    // CLOSE = span dropped (boundary marker for "completed", carries
    // the framework's own elapsed time and any fields recorded over
    // the lifetime of the span).
    .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
    .init();
```

What this actually buys us:

- `FmtSpan::NEW` fires when the span is *constructed*, before any
  body runs. Fields evaluated in `#[instrument(fields(...))]` (the
  request-side fields like `method`, `url`, `request_headers`) are
  populated and visible at this point. Fields declared as
  `field::Empty` are not yet recorded and render as their empty
  marker.
- `FmtSpan::CLOSE` fires when the span is dropped. Late-recorded
  fields (`status`, `response_headers`, `response_body_bytes`,
  `tree_ref`, etc.) are visible at this point because the subscriber
  re-reads the span's recorded fields. Elapsed time is reported by
  the framework.
- `tracing_subscriber::fmt::Layer` interleaves an event's own fields
  with the *current span chain's* fields when rendering, so an inner
  executor span sees the outer `callout` span's
  `operation_id`/`callout_index`/`kind` automatically.

`FmtSpan::NEW` is span *creation*, not span *enter*. We do not enable
`ENTER`/`EXIT` because every `.await` that yields and resumes within
an instrumented async fn would emit an enter/exit pair, and the
output gets noisy fast for callouts that yield several times.

**Things that get deleted in this commit**:

- `log_callout_response` (and the `&Callout` parameter from 2.3 — the
  central response logger is gone, blob.fetch's `cache_key` lives on
  the executor's own span).
- The `Callouts::execute_*` per-arm `info!("callout started")` /
  `info!("callout response")` blocks (already gone after 2.8 if that
  ran first; if 2.6 lands before 2.8, drop the duplication here).
- Any helper still named `log_*_response` from previous phases.

**Executor signature consolidation** (also in this commit):

The `#[instrument]` macro extracts fields from the request struct, so
each executor method takes the WIT request type directly. This is a
small follow-on rename from 2.1 / 2.2:

- `ArchiveExecutor::open_archive(blob, format, strip_prefix)` →
  `ArchiveExecutor::open(req: &wit_types::ArchiveOpenRequest)`.
- `BlobExecutor::read_blob(blob, offset, len)` →
  `BlobExecutor::read(req: &wit_types::ReadBlobRequest)`.
- `GitExecutor::open_repo(cache_key, clone_url)` →
  `GitExecutor::open_repo(req: &wit_types::GitOpenRequest)`.

`HttpExecutor::fetch` and `BlobExecutor::fetch` already take their
request structs after 2.2.

**Why this is finally idiomatic**:

- One central match over `Callout` in `run_callout`. No parallel
  matches for log emission.
- Each executor's observability lives with its execution code. Add a
  callout kind: add a `run_callout` arm + a new executor method with
  `#[instrument]`. Done. No central log table to update.
- Span NEW/CLOSE events are the canonical way to mark "thing
  started" / "thing completed" in `tracing`. Don't manually emit
  `info!("started")`.
- `field::Empty` + `Span::current().record(...)` is the canonical
  late-binding pattern.
- `LogUrl` / `LogHeaders` / `WitHeaders` continue to do redaction
  (lazy `Display`).
  Nothing else hand-rolls field plumbing.

**Behavior change call-out**:

This is a log-format change:

- `callout_kind` is now `kind` (outer span attribute).
- `"callout started"` / `"callout response"` / `"callout error"`
  message events are gone. Span NEW/CLOSE events from the
  subscriber's `FmtSpan::NEW | FmtSpan::CLOSE` config replace them.
- Each callout now emits two spans: an outer `callout` span carrying
  cross-cutting context, and an inner executor span (e.g. `fetch`,
  `open_repo`) carrying kind-specific fields. After 2.7, unsupported
  variants also go through an instrumented `unsupported_callout` helper
  so they have the same outer/inner span structure. Field rendering
  depends on the subscriber.
- Elapsed time comes from the framework's span timing rather than a
  manually-recorded `elapsed_us` field.
- Error fields are renamed from the current flat event fields
  `kind`/`error`/`retryable` to span fields
  `error.kind`/`error.message`/`error.retryable`.

If any external tooling (Grafana, log shipper, regex) parses the old
flat format, file a follow-up to add a JSON layer behind a flag and
migrate that tooling to the structured stream. Do not block this PR
on tooling that's currently parsing dev-mode plain-text output.

**Verify**:

- `just check`.

- **Snapshot test** (required, lands in the same commit) using
  `tracing-subscriber`'s test layer to capture rendered events:

  ```rust
  // crates/host/tests/callout_tracing.rs
  use tracing_subscriber::fmt::{format::FmtSpan, MakeWriter};
  // Build a fmt layer writing to an in-memory buffer; configure with
  // FmtSpan::NEW | FmtSpan::CLOSE; drive a small test-only helper that
  // instruments a canned future/result exactly like dispatch_one does;
  // assert the captured bytes contain:
  //   - one "new" line with kind, method, url, request_headers
  //   - one "close" line with status, response_headers, response_body_bytes
  //   - operation_id and callout_index on both lines
  //   - <redacted> wherever a sensitive header / query param was set
  ```

  Cover at least: `Fetch`, `FetchBlob` (verify `cache_key` and
  late-recorded `blob`), `GitOpenRepo` (verify `tree_ref` lands at
  close), `OpenArchive`, `ReadBlob`, and one `Unsupported` variant
  (verify the warn fires with `unsupported_variant` populated and a
  `callout-error` is returned).

- `RUST_LOG=info just dev` smoke pass: exercise one real callout of
  each kind through the FUSE mount; confirm `/tmp/omnifs.log` shows
  both `new` and `close` records per callout with the redacted URL
  and headers visible.

- Run the existing `callout_log_tests` module — it tests redaction,
  not field shape, so it should still pass unchanged.

---

### 2.7 Make the unsupported-variant arm noisy and exhaustive

**Files**: where `run_callout` lives.

**Approach**: Today the catch-all silently produces an internal error.
After 2.5, `CalloutKind::Unsupported` covers the seven WIT-defined
variants the runtime doesn't yet implement (`stream-*`, `ws-*`).
Update `run_callout` to match those variants explicitly (no wildcard
arm) and route them through an instrumented helper that emits a `warn!`
and records the returned `callout-error`. Exhaustive matching also
means a future WIT-added variant fails to compile here rather than
silently falling through.

```rust
async fn run_callout(&self, callout: &wit_types::Callout) -> wit_types::CalloutResult {
    match callout {
        wit_types::Callout::Fetch(req)        => self.http.fetch(req).await,
        wit_types::Callout::FetchBlob(req)    => self.blob.fetch(req).await,
        wit_types::Callout::GitOpenRepo(req)  => self.git.open_repo(req),
        wit_types::Callout::OpenArchive(req)  => self.archive.open(req).await,
        wit_types::Callout::ReadBlob(req)     => self.blob.read(req),
        wit_types::Callout::StreamOpen(_)
        | wit_types::Callout::StreamRecv(_)
        | wit_types::Callout::StreamClose(_)
        | wit_types::Callout::WsConnect(_)
        | wit_types::Callout::WsSend(_)
        | wit_types::Callout::WsRecv(_)
        | wit_types::Callout::WsClose(_) => self.unsupported_callout(callout),
    }
}

#[tracing::instrument(target = "omnifs_callout", skip_all, fields(
    unsupported_variant = unsupported_callout_variant(callout),
    error.kind = tracing::field::Empty,
    error.message = tracing::field::Empty,
    error.retryable = tracing::field::Empty,
))]
fn unsupported_callout(&self, callout: &wit_types::Callout) -> wit_types::CalloutResult {
    let variant = unsupported_callout_variant(callout);
    tracing::warn!(
        target: "omnifs_callout",
        variant,
        "callout variant not implemented",
    );
    let result = callout_internal("callout type not yet implemented");
    record_outcome(&result);
    result
}

fn unsupported_callout_variant(callout: &wit_types::Callout) -> &'static str {
    match callout {
        wit_types::Callout::StreamOpen(_) => "stream.open",
        wit_types::Callout::StreamRecv(_) => "stream.recv",
        wit_types::Callout::StreamClose(_) => "stream.close",
        wit_types::Callout::WsConnect(_) => "ws.connect",
        wit_types::Callout::WsSend(_) => "ws.send",
        wit_types::Callout::WsRecv(_) => "ws.recv",
        wit_types::Callout::WsClose(_) => "ws.close",
        _ => "unknown",
    }
}
```

**Verify**: `just check`. Confirm `cargo check` rejects a hypothetical
new `Callout` variant by temporarily adding one to the WIT and
observing the missing-arm compile error (then revert).

---

### 2.8 Delete `Callouts` struct; inline dispatch on `ProviderRuntime`

**Files**: `crates/host/src/runtime/mod.rs`.

**Approach**: `Callouts` exists for one method called from one place.
Inline. Move the empty-callouts check to `drive_provider`.

```rust
impl ProviderRuntime {
    async fn dispatch_callouts(
        &self,
        operation_id: u64,
        callouts: &[wit_types::Callout],
    ) -> Vec<wit_types::CalloutResult> {
        let futures = callouts.iter().enumerate()
            .map(|(index, callout)| self.dispatch_one(operation_id, index, callout));
        futures::future::join_all(futures).await
    }

    async fn run_callout(&self, callout: &wit_types::Callout) -> wit_types::CalloutResult {
        match callout {
            wit_types::Callout::Fetch(req)        => self.http.fetch(req).await,
            wit_types::Callout::FetchBlob(req)    => self.blob.fetch(req).await,
            wit_types::Callout::GitOpenRepo(req)  => self.git.open_repo(req),
            wit_types::Callout::OpenArchive(req)  => self.archive.open(req).await,
            wit_types::Callout::ReadBlob(req)     => self.blob.read(req),
            wit_types::Callout::StreamOpen(_)
            | wit_types::Callout::StreamRecv(_)
            | wit_types::Callout::StreamClose(_)
            | wit_types::Callout::WsConnect(_)
            | wit_types::Callout::WsSend(_)
            | wit_types::Callout::WsRecv(_)
            | wit_types::Callout::WsClose(_) => self.unsupported_callout(callout),
        }
    }
}
```

`unsupported_callout` is the helper added in 2.7. Empty check moves
into `drive_provider`:

```rust
wit_types::ProviderStep::Suspended(callouts) => {
    if callouts.is_empty() {
        return Err(RuntimeError::ProviderProtocol(
            "provider suspended with no callouts".to_string(),
        ));
    }
    let results = self.dispatch_callouts(id, &callouts).await;
    step = self.resume_provider(id, results)?;
},
```

Delete the `Callouts` struct, its `new`, `execute`, `execute_one`, and
five `execute_*` methods.

**Verify**: `just check`. End-to-end FUSE smoke test.

---

### 2.9 Move `Op::execute` to `ProviderRuntime::start_op`; fold into `run_op`

**Files**: `crates/host/src/runtime/mod.rs`,
`crates/host/src/runtime/browse_pipeline.rs`.

**Approach**: `Op::execute(&self, runtime, id)` is a backwards
dependency. Make it `ProviderRuntime::start_op(&self, op: &Op, id: u64)`.
Then absorb the two-step `start_op + drive_provider` into a single
`run_op` for ops that may suspend.

**Initialize stays separate.** `ProviderRuntime::initialize()` today
explicitly rejects suspension (`"initialize suspended with callouts"`).
That semantics must be preserved — DO NOT route Initialize through
`run_op`. Use `start_op` for Initialize and reject the `Suspended` arm
locally.

```rust
impl ProviderRuntime {
    fn start_op(&self, op: &Op, id: u64) -> Result<wit_types::ProviderStep> {
        let mut store = self.store.lock();
        let browse = self.bindings.omnifs_provider_browse();
        match op {
            Op::LookupChild { parent_path, name } => browse.call_lookup_child(&mut *store, id, parent_path, name).map_err(Into::into),
            Op::ListChildren { path }             => browse.call_list_children(&mut *store, id, path).map_err(Into::into),
            Op::ReadFile { path }                 => browse.call_read_file(&mut *store, id, path).map_err(Into::into),
            Op::OpenFile { path }                 => browse.call_open_file(&mut *store, id, path).map_err(Into::into),
            Op::ReadChunk { handle, offset, length } => browse.call_read_chunk(&mut *store, id, *handle, *offset, *length).map_err(Into::into),
            Op::Initialize => Ok(wit_types::ProviderStep::Returned(
                self.bindings.omnifs_provider_lifecycle().call_initialize(&mut *store, &self.config_bytes)?,
            )),
            Op::OnEvent { event } => self.bindings.omnifs_provider_notify().call_on_event(&mut *store, id, event).map_err(Into::into),
        }
    }

    pub(super) async fn run_op(&self, op: Op) -> Result<wit_types::OpResult> {
        let id = self.operation_ids.allocate();
        let mut step = self.start_op(&op, id)?;
        loop {
            match step {
                wit_types::ProviderStep::Returned(ret) => {
                    return self.finish_provider_return(&op, ret);
                },
                wit_types::ProviderStep::Suspended(callouts) => {
                    if callouts.is_empty() {
                        return Err(RuntimeError::ProviderProtocol(
                            "provider suspended with no callouts".to_string(),
                        ));
                    }
                    let results = self.dispatch_callouts(id, &callouts).await;
                    step = self.resume_provider(id, results)?;
                },
            }
        }
    }
}
```

`initialize()` keeps its existing shape, just routed through `start_op`:

```rust
pub fn initialize(&self) -> Result<wit_types::OpResult> {
    let id = self.operation_ids.allocate();
    let op = Op::Initialize;
    match self.start_op(&op, id)? {
        wit_types::ProviderStep::Returned(ret) => self.finish_provider_return(&op, ret),
        wit_types::ProviderStep::Suspended(_) => Err(RuntimeError::ProviderProtocol(
            "initialize suspended with callouts".to_string(),
        )),
    }
}
```

Delete `impl Op { fn execute }`, `drive_provider`, `call_provider_op`.
Update `browse_pipeline.rs` to call `self.run_op(op)` directly for
browse ops and `OnEvent`. Initialize stays out of `run_op`.

`call_timer_tick` becomes:

```rust
pub async fn call_timer_tick(&self) -> Result<wit_types::OpResult> {
    let active_paths = self.activity_table.lock().active_path_sets();
    self.run_op(Op::OnEvent {
        event: wit_types::ProviderEvent::TimerTick(wit_types::TimerTickContext { active_paths }),
    }).await
}
```

**Verify**: `just check`. Confirm Initialize stays outside `run_op` and
keeps the existing suspended guard while it still routes through
`start_op`. Do not invent a broad test seam just to synthesize this
currently impossible branch; Phase 5.0 removes the fake ProviderStep
wrapper around lifecycle initialize. All browse/lifecycle paths still
respect their existing suspension policies.

---

## Phase 3: Effect-handling refactor

### 3.1 Split `apply_effects` into accumulator + helpers

**Files**: `crates/host/src/runtime/mod.rs` (or `effects.rs` after 5.4).

**Approach**: Pull the dirent-merge pass out from the project-effect
loop. Extract a `ProjectionAccumulator` so the top-level function reads
linearly.

```rust
#[derive(Default)]
struct ProjectionAccumulator {
    dirs: std::collections::BTreeSet<String>,
    children: std::collections::BTreeMap<String, std::collections::BTreeMap<String, DirentRecord>>,
}

impl ProjectionAccumulator {
    fn add(&mut self, entry: &wit_types::ProjEntry, batch: &mut Vec<BatchRecord>) {
        if matches!(entry.kind, wit_types::EntryKind::Directory) {
            self.dirs.insert(entry.path.clone());
        }
        if let Some((parent, name)) = split_projected_path(&entry.path) {
            let name = name.to_string();
            self.children.entry(parent.to_string()).or_default().insert(
                name.clone(),
                DirentRecord { name, meta: EntryMeta::from(&entry.kind) },
            );
        }
        ProviderRuntime::push_projected_entry(batch, &entry.path, &entry.kind);
        if let wit_types::EntryKind::File(file) = &entry.kind {
            ProviderRuntime::push_projected_file_content(batch, &entry.path, file);
        }
    }
}

impl ProviderRuntime {
    pub(super) fn apply_effects(&self, effects: &[wit_types::Effect]) {
        let mut batch = Vec::new();
        let mut projections = ProjectionAccumulator::default();
        for effect in effects {
            match effect {
                wit_types::Effect::Project(entry) => projections.add(entry, &mut batch),
                wit_types::Effect::InvalidatePath(path) => {
                    self.cache_delete_path(path);
                    self.invalidation.record_path(path.clone());
                },
                wit_types::Effect::InvalidatePrefix(prefix) => {
                    self.cache_delete_prefix(prefix);
                    self.invalidation.record_prefix(prefix.clone());
                },
                wit_types::Effect::DisownTree(_) => {},
            }
        }
        self.merge_projected_dirs(projections, &mut batch);
        if !batch.is_empty() {
            tracing::debug!(target: "omnifs_cache", kind = "project", count = batch.len(), "applying projection effects");
            self.cache_put_batch(&batch);
        }
    }

    fn merge_projected_dirs(&self, projections: ProjectionAccumulator, batch: &mut Vec<BatchRecord>) {
        let ProjectionAccumulator { dirs, mut children } = projections;
        for dir in dirs {
            let Some(new_children) = children.remove(&dir) else { continue };
            let (previously_exhaustive, mut existing) = self
                .cache_get(&dir, RecordKind::Dirents, None)
                .and_then(|record| DirentsPayload::deserialize(&record.payload))
                .map_or_else(
                    || (false, std::collections::BTreeMap::new()),
                    |payload| (
                        payload.exhaustive,
                        payload.entries.into_iter().map(|e| (e.name.clone(), e)).collect(),
                    ),
                );
            let introduced = new_children.keys().any(|n| !existing.contains_key(n));
            existing.extend(new_children);
            if let Some(payload) = (DirentsPayload {
                entries: existing.into_values().collect(),
                exhaustive: previously_exhaustive && !introduced,
            }).serialize() {
                batch.push(BatchRecord::new(
                    dir, RecordKind::Dirents, None,
                    CacheRecord::new(RecordKind::Dirents, payload),
                ));
            }
        }
    }
}
```

**Verify**: `just check`. The cache test that exercises projection
merging must pass.

---

## Phase 4: Error type split

### 4.1 Extract `RuntimeBuildError` from `RuntimeError`

**Files**: `crates/host/src/runtime/mod.rs` plus any `Result<_, RuntimeError>`
return paths from `ProviderRuntime::new` and its callers.

**Approach**:

```rust
#[derive(Debug, thiserror::Error)]
pub enum RuntimeBuildError {
    #[error("wasmtime: {0}")]
    Wasmtime(#[from] wasmtime::Error),
    #[error("http client: {0}")]
    HttpClient(#[from] reqwest::Error),
    #[error("invalid config: {0}")]
    InvalidConfig(String),
    #[error("provider protocol: {0}")]
    ProviderProtocol(String),
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("wasmtime: {0}")]
    Wasmtime(#[from] wasmtime::Error),
    #[error("provider protocol: {0}")]
    ProviderProtocol(String),
    #[error("provider returned error: {0:?}")]
    ProviderError(wit_types::ProviderError),
    #[error("{op:?} returned unexpected result: {result:?}")]
    UnexpectedOpResult { op: Box<Op>, result: Box<wit_types::OpResult> },
}
```

`pub fn new(...) -> Result<Self, RuntimeBuildError>`.
All other public methods continue to return `Result<T, RuntimeError>`.

Update callers in `cli`/`host` that consumed `RuntimeError::HttpClient`
or `RuntimeError::InvalidConfig` to handle `RuntimeBuildError` from the
construction path.

**Verify**: `just check`. The error-flow tests still classify
construction errors vs runtime errors correctly.

---

## Phase 5: Module reorganization

After Phase 4 the file is structurally cleaner but still ~1000+ lines.
Split it. Step 5.0 establishes a real type-level boundary first; the
rest are pure file moves plus visibility adjustments — no logic
changes.

### 5.0 Extract `ProviderInstance` (Wasmtime mechanics boundary)

**Files**:
- New `crates/host/src/runtime/instance.rs`.
- `crates/host/src/runtime/mod.rs` (`ProviderRuntime` shrinks; holds
  `instance: ProviderInstance` instead of bare `store`/`bindings`/
  `config_bytes`).

**Approach**: today's `ProviderRuntime` mixes Wasmtime instance
mechanics (store, bindings, config bytes, lifecycle calls, in-process
op invocation) with host runtime orchestration (executors, caches,
activity, invalidation, inflight). That's a god-object. Split along
the natural seam: `ProviderInstance` owns everything that talks
directly to the WASM component; `ProviderRuntime` owns orchestration.

Doing this BEFORE the file moves (5.1+) means `instance.rs` lands
together with the other extractions; doing it after means `mod.rs`
gets reorganized twice.

```rust
// crates/host/src/runtime/instance.rs
pub struct ProviderInstance {
    store: parking_lot::Mutex<wasmtime::Store<HostState>>,
    bindings: crate::Provider,
    config_bytes: Vec<u8>,
}

impl ProviderInstance {
    pub fn new(/* engine, wasm_path, config_bytes, ... */) -> Result<Self, RuntimeBuildError> { ... }

    pub fn start_op(&self, op: &Op, id: u64) -> Result<wit_types::ProviderStep, RuntimeError> {
        let mut store = self.store.lock();
        let browse = self.bindings.omnifs_provider_browse();
        match op {
            Op::LookupChild { parent_path, name } => browse.call_lookup_child(&mut *store, id, parent_path, name).map_err(Into::into),
            // ... rest of start_op body from 2.9 ...
        }
    }

    pub fn resume(&self, id: u64, results: Vec<wit_types::CalloutResult>) -> Result<wit_types::ProviderStep, RuntimeError> {
        let mut store = self.store.lock();
        Ok(self.bindings.omnifs_provider_continuation().call_resume(&mut *store, id, &results)?)
    }

    pub fn initialize(&self) -> Result<wit_types::OpResult, RuntimeError> {
        let mut store = self.store.lock();
        Ok(self.bindings.omnifs_provider_lifecycle().call_initialize(&mut *store, &self.config_bytes)?)
    }

    pub fn shutdown(&self) -> Result<(), RuntimeError> {
        let mut store = self.store.lock();
        self.bindings.omnifs_provider_lifecycle().call_shutdown(&mut *store)?;
        Ok(())
    }

    pub fn config_schema(&self) -> Result<Option<String>, RuntimeError> {
        let mut store = self.store.lock();
        Ok(self.bindings.omnifs_provider_lifecycle().call_get_config_schema(&mut *store)?)
    }

    pub fn capabilities(&self) -> Result<wit_types::RequestedCapabilities, RuntimeError> {
        let mut store = self.store.lock();
        Ok(self.bindings.omnifs_provider_lifecycle().call_capabilities(&mut *store)?)
    }

    pub fn close_file(&self, handle: u64) -> Result<(), RuntimeError> {
        let mut store = self.store.lock();
        self.bindings.omnifs_provider_browse().call_close_file(&mut *store, handle)?;
        Ok(())
    }
}
```

`ProviderRuntime` shrinks accordingly:

```rust
pub struct ProviderRuntime {
    instance: ProviderInstance,           // was: store + bindings + config_bytes
    operation_ids: OperationIds,
    http: HttpExecutor,                   // becomes HttpStack in Phase 6
    git: GitExecutor,
    blob: BlobExecutor,
    archive: Arc<ArchiveExecutor>,
    blob_cache: Arc<BlobCache>,
    trees: Arc<TreeRefs>,
    l2: Option<cache::l2::Cache>,
    invalidation: InvalidationState,
    activity_table: Mutex<ActivityTable>,
    declared_handlers: Vec<DeclaredHandler>,
    inflight: InFlight,
}
```

`run_op` calls `self.instance.start_op(...)` and `self.instance.resume(...)`.
`initialize()` calls `self.instance.initialize()` as a direct lifecycle
call, then still routes the returned `OpResult` through
`finish_provider_return(&Op::Initialize, ...)` for validation. The fake
initialize operation id from the pre-5.0 shape disappears here because
`call_initialize` does not take an id and cannot suspend. Lifecycle
methods (`shutdown`, `config_schema`, `capabilities`, `call_close_file`)
become one-line passthroughs to `instance`.

**Drop the dead `Op::Initialize` arm from `start_op`.** Because
`ProviderInstance::initialize` is now the only path that drives
provider initialization, no caller ever passes `Op::Initialize` to
`start_op`. The 2.9 body still has that arm; remove it here so the
match no longer carries unreachable code (clippy will flag it
otherwise). The `Op::Initialize` variant itself stays in the enum
because `finish_provider_return` and the validator still tag
initialize results with it.

**Verify**: `just check`. End-to-end FUSE smoke test.

---

### 5.1 Move log redaction to `runtime/log_redaction.rs`

**Move**: `LogUrl`, `LogHeaders`, `WitHeaders`, `is_sensitive_header`,
`is_sensitive_query_param`, `write_truncated_for_log`, plus the
`callout_log_tests` module.

**Visibility**: `pub(super) struct LogUrl<'a>(pub(super) &'a str);` etc.

Add `mod log_redaction;` in `runtime/mod.rs` and import as needed.

**Verify**: `just check`. Tests run from the new module.

---

### 5.2 Move WIT↔cache `From` impls to `runtime/wit_conversions.rs`

**Move** these `impl` blocks:
- `From<&wit_types::FileProj> for cache::FileAttrsCache`
- `From<&wit_types::FileAttrs> for cache::FileAttrsCache`
- `From<&wit_types::FileSize> for SizeCache`
- `From<&wit_types::ProjBytes> for cache::BytesCache`
- `From<wit_types::ReadMode> for cache::ReadModeCache`
- `From<wit_types::Stability> for cache::StabilityCache`
- `From<&wit_types::EntryKind> for EntryMeta`
- `From<&wit_types::EntryKind> for cache::EntryKindCache`
- `From<ErrorKind> for wit_types::ErrorKind`
- `From<wit_types::ArchiveFormat> for ArchiveFormat`

`pub(super) mod wit_conversions;` in `runtime/mod.rs`. The orphan rule
is satisfied because both sides are crate-local.

**Verify**: `just check`.

---

### 5.3 (REMOVED — folded into 7.1)

Original intent was to move `Validator` to a standalone
`runtime/validator.rs`. With the revised Phase 7 (co-locate `Op` +
`Validator` since they're tightly coupled), this happens in 7.1
instead. Skip this step.

---

### 5.4 Move effect application to `runtime/effects.rs`

**Move**: `apply_effects`, `merge_projected_dirs`,
`ProjectionAccumulator`, `push_projected_file_content`,
`push_projected_entry`, `split_projected_path`.

These become `pub(super)` free functions or remain methods on
`ProviderRuntime` via `impl ProviderRuntime` in the new file
(Rust permits inherent impls in any module of the same crate).

Recommended: put `apply_effects` and `merge_projected_dirs` in a new
`impl ProviderRuntime` block inside `effects.rs`; the `push_projected_*`
helpers become free functions in that module since they don't use `self`.

**Verify**: `just check`. Effect tests still pass.

---

### 5.5 Move callout dispatch to `runtime/callouts.rs`

**Move**: `dispatch_callouts`, `dispatch_one`, `run_callout`,
`unsupported_callout`, `unsupported_callout_variant`, `CalloutKind`,
`record_outcome`, the constructor helpers
(`callout_error` / `callout_internal` / ...).

The per-kind log helpers (`log_callout_response`, `log_request_fields`,
etc.) no longer exist — Phase 2.6 replaced them with `#[instrument]`
on each executor. So this step only moves the central dispatch.

Same pattern — `impl ProviderRuntime` in `callouts.rs`.

After this, `runtime/mod.rs` contains only `ProviderRuntime` struct
definition, `RuntimeError` / `RuntimeBuildError` types, public
lifecycle entry points that delegate to `ProviderInstance` (`new`,
`initialize`, `shutdown`, `config_schema`, `capabilities`,
`call_close_file`, `call_timer_tick`), the cache get/put accessors,
`run_op`, `finish_provider_return`, `read_blob_full`,
`resolve_tree_ref`, `absolute_mount_path`.

`HostState`, `start_op`, and `resume` live in `runtime/instance.rs`
(per 5.0). The `Op` enum and `Validator` live in `runtime/op.rs`
(per 7.1).

Target: `runtime/mod.rs` ≤ 600 lines.

**Verify**: `just check`. End-to-end FUSE smoke test.

---

## Phase 6: HTTP/Blob executor merge

### 6.1 Introduce `HttpStack`

**Files**:
- New: `crates/host/src/runtime/http_stack.rs`.
- `crates/host/src/runtime/executor.rs` (slim down or delete; if
  `HttpExecutor` had any other purpose, fold here).

**Approach**: `HttpStack` owns the reqwest client, auth, and
capability checker. The public API is `send(...) -> Result<reqwest::Response, CalloutResult>`,
which handles auth, capability, method parsing, header construction,
body, and network error mapping in one place.

**Precise encapsulation contract**: `reqwest::Client`,
`reqwest::Method`, `reqwest::header::HeaderMap`, and
`reqwest::RequestBuilder` stay hidden inside `HttpStack`.
`reqwest::Response` *intentionally* crosses the boundary so
`BlobExecutor` can stream `response.bytes_stream()` to disk without
re-buffering. This is the only reqwest type the caller ever sees.

`HttpStack::fetch` decodes the response body to memory.
`BlobExecutor` calls `send` and streams the body to disk. Same
guarantee path on both, auth/capability semantics can't drift.
Keep the 2.6 `#[instrument(target = "omnifs_callout", ...)]`
annotation on `HttpStack::fetch`; `send` is shared transport plumbing,
not a callout span.

```rust
pub struct HttpStack {
    client: reqwest::Client,
    auth: std::sync::Arc<crate::auth::AuthManager>,
    capability: std::sync::Arc<crate::runtime::capability::CapabilityChecker>,
}

impl HttpStack {
    pub fn new(
        auth: std::sync::Arc<crate::auth::AuthManager>,
        capability: std::sync::Arc<crate::runtime::capability::CapabilityChecker>,
        timeout: std::time::Duration,
    ) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .user_agent("omnifs")
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(timeout)
            .build()?;
        Ok(Self { client, auth, capability })
    }

    /// Authorize, build, and dispatch a request. Returns the in-flight
    /// response on success or a fully-formed CalloutResult on any
    /// pre-flight or network failure. `reqwest::Response` is the only
    /// reqwest type the caller sees.
    pub async fn send(
        &self,
        method: &str,
        url: &str,
        headers: &[wit_types::Header],
        body: Option<&[u8]>,
    ) -> Result<reqwest::Response, wit_types::CalloutResult> {
        self.capability.check_url(url).map_err(|e| callout_denied(e.to_string()))?;

        let auth_headers = self.auth.headers_for_url(url);
        if auth_headers.is_empty() && self.auth.requires_auth_for_url(url) {
            return Err(callout_denied(format!("no credentials for {url}")));
        }

        let reqwest_method = reqwest::Method::from_str(method)
            .map_err(|_| callout_denied(format!("unsupported HTTP method: {method}")))?;

        let header_map = crate::runtime::http_headers::build_header_map(
            auth_headers.iter().map(|(n, v)| (n.as_str(), v.as_str())),
            headers.iter().map(|h| (h.name.as_str(), h.value.as_str())),
        ).map_err(callout_internal)?;

        let mut request = self.client.request(reqwest_method, url).headers(header_map);
        if let Some(body) = body {
            request = request.body(body.to_vec());
        }

        request.send().await.map_err(|e| callout_network(e.to_string()))
    }

    /// In-memory fetch: send + decode body.
    pub async fn fetch(&self, req: &wit_types::HttpRequest) -> wit_types::CalloutResult {
        let result = match self.send(&req.method, &req.url, &req.headers, req.body.as_deref()).await {
            Ok(response) => {
                let status = response.status().as_u16();
                let headers = crate::runtime::http_headers::decode_response_headers(response.headers());
                match response.bytes().await {
                    Ok(body) => wit_types::CalloutResult::HttpResponse(wit_types::HttpResponse {
                        status,
                        headers: headers.into_iter().map(|(name, value)| wit_types::Header { name, value }).collect(),
                        body: body.to_vec(),
                    }),
                    Err(e) => callout_network(e.to_string()),
                }
            },
            Err(early) => early,
        };
        record_outcome(&result);
        result
    }
}
```

`BlobExecutor` calls `self.http.send(...)`, gets a `reqwest::Response`,
streams the body to disk via the existing `stream_response_body`
helper. The blob executor never directly owns a reqwest client or a
header map.
Keep the 2.6 `#[instrument(target = "omnifs_callout", ...)]`
annotations on `BlobExecutor::fetch` and `BlobExecutor::read`.

`ProviderRuntime` holds an `Arc<HttpStack>` and the blob executor
holds another `Arc<HttpStack>` (or the same one if/when timeouts
become per-call — see the configured-instances note below).

**Verify**: `just check`. HTTP tests pass. `grep -n 'reqwest::' crates/host/src/runtime/blob.rs`
may still show `reqwest::Response` in `BlobExecutor::fetch`/tests, but
must not show `Client`, `Method`, `RequestBuilder`, or `header::*`.

---

### 6.2 Refactor `BlobExecutor` to delegate to `HttpStack`

**Files**: `crates/host/src/runtime/blob.rs`.

**Approach**: Drop the blob executor's own reqwest client, auth, and
capability checker fields. Hold an `Arc<HttpStack>`. Use
`http_stack.send(...)` to dispatch the request, then stream the
returned `reqwest::Response` body to disk.

```rust
pub struct BlobExecutor {
    http: std::sync::Arc<HttpStack>,
    cache: std::sync::Arc<BlobCache>,
    limits: BlobLimits,
}

impl BlobExecutor {
    pub fn new(http: std::sync::Arc<HttpStack>, cache: std::sync::Arc<BlobCache>, limits: BlobLimits) -> Self {
        Self { http, cache, limits }
    }

    pub async fn fetch(&self, req: &wit_types::BlobFetchRequest) -> wit_types::CalloutResult {
        let result = if !is_safe_path_segment(&req.cache_key) {
            callout_invalid(format!("cache key {} is unsafe", req.cache_key))
        } else {
            let lock = self.cache.key_lock(&req.cache_key);
            let _guard = lock.lock().await;
            if let Some(record) = self.cache.lookup_by_key(&req.cache_key) {
                blob_fetched_to_wit(&record)
            } else {
                // HttpStack::send already returns a fully-formed CalloutResult on
                // pre-flight or network failure — pass it through unchanged.
                match self.http.send(&req.method, &req.url, &req.headers, req.body.as_deref()).await {
                    Ok(response) => match self.materialize(&req.cache_key, response).await {
                        Ok(record) => blob_fetched_to_wit(&record),
                        Err(e) => e.into(),
                    },
                    Err(early) => early,
                }
            }
        };
        record_outcome(&result);
        result
    }

    /// Stream the response body to disk and persist the cache record.
    /// Internal — keeps using typed BlobError so each helper composes
    /// without leaking CalloutResult construction.
    async fn materialize(&self, cache_key: &str, response: reqwest::Response)
        -> Result<BlobRecord, BlobError>
    {
        // ... existing stream_response_body + metadata + persist logic ...
    }

    pub fn read(&self, req: &wit_types::ReadBlobRequest) -> wit_types::CalloutResult {
        // ... existing read_blob body, using req.blob / req.offset / req.len
        // and returning wit_types::CalloutResult directly ...
    }
}
```

`ProviderRuntime::new` builds two `HttpStack` instances of the same
shared type — one per timeout regime — and hands each to its
consumer. The win isn't a single instance; it's a single
implementation, so auth and capability semantics can't drift between
the HTTP and blob paths.

```rust
let http = std::sync::Arc::new(HttpStack::new(
    auth.clone(), capability.clone(), std::time::Duration::from_secs(30),
)?);
let blob_http = std::sync::Arc::new(HttpStack::new(
    auth.clone(), capability.clone(), std::time::Duration::from_secs(120),
)?);
let blob = BlobExecutor::new(blob_http, blob_cache.clone(), blob_limits);
```

If a future change converts the timeout to a per-call parameter
(`HttpStack::send` or `HttpStack::fetch` taking `timeout: Duration`),
collapse to one instance then. Not in scope for this PR.

Delete `HttpExecutor` entirely if `executor.rs` becomes empty after
this. `ErrorKind` enum still lives there if anywhere else uses it; if
not, fold into `callouts.rs`.

**Verify**: `just check`. Blob fetch and HTTP fetch integration tests
pass. `ProviderRuntime` no longer has two parallel auth/capability
chains.

---

## Phase 7: Co-locate `Op` and `Validator` (no trait)

**Decision (revised)**: drop the trait-based `OpRequest` rewrite from
the original plan. Reason: `coalesced` and `InFlight` are intentionally
`OpResult`-shaped (`SharedOutcome = Result<wit_types::OpResult, String>`
in `inflight.rs`); pushing typed extraction up the stack forces a
two-tier `run_op` / `run_op_raw` split or a generalization of the
inflight machinery that buys nothing concrete. The current enum +
match dispatch is the right shape for this layer. Keep it.

What remains in this phase is just module hygiene: `Op` and
`Validator` are tightly coupled (Validator dispatches on `&Op`
variants), so move them together into one module.

### 7.1 Move `Op` enum and `Validator` to `runtime/op.rs`

**Files**:
- New `crates/host/src/runtime/op.rs` containing `enum Op`,
  `Validator`, `Validator::returned`, all validator helper methods,
  the `attr_contract_tests` module, and the `validate_operation_result`
  / `validate_return` test helpers.
- `crates/host/src/runtime/mod.rs` (delete the moved items, add
  `pub(super) mod op;`).

**Approach**: Pure file move. No logic changes. Visibility on `Op` and
`Validator` becomes `pub(super)`. Anything that was `pub(crate)` stays
`pub(crate)`.

`start_op` does NOT move into `op.rs` — it landed on
`ProviderInstance` in 5.0 (which lives in `runtime/instance.rs`).
`op.rs` is data + validation only.

**Verify**: `just check`. `attr_contract_tests` still runs from the
new module. No call site outside `runtime/` should care.

---

## Deferred analysis: Trait-based `OpRequest` (not in this PR)

Original analysis kept here as a record. **Do not implement.**

### Why deferred

The trait would require either:
- generalizing `coalesced`/`InFlight` (`SharedOutcome`,
  `share_outcome`/`unshare_outcome`) to be parametric over the future
  output type — non-trivial given the broadcast channel carries
  `OpResult` so all subscribers see the same shape; or
- introducing a `run_op` (typed) / `run_op_raw` (`OpResult`) dual layer
  where coalescing uses the raw form and callers re-extract — adds an
  abstraction without removing one.

Neither delivers a clean enough win to justify the churn. The current
enum + per-method wrapper boilerplate in `browse_pipeline.rs` is
modest after 1.x and 2.x land. Revisit only if a future change forces
the issue (e.g. typed callouts inside `coalesced`).

The original sketch introduced an `OpRequest` trait, per-op structs, a
generic `run_op`, a parametric validator, and eventual deletion of
`Op`. Those code sketches are intentionally removed from this plan:
they were stale after the `ProviderInstance` extraction and are not a
target for this PR.

If this is revisited later, design it fresh against the post-refactor
shape. In particular, account for:

- `ProviderInstance` owning `start_op`/`resume`.
- `coalesced`/`InFlight` sharing `wit_types::OpResult`.
- `Validator` depending on `Op` for subtree handoff path checks.

---

## Phase 8: Confirmation-required cleanups

These need user confirmation before executing. Stop and ask.

### 8.1 Delete dead WIT variants — **CONFIRM**

**Files**: `wit/*.wit`, plus generated bindings consumers.

**Variants/interfaces**: `OpResult::PlanMutations`, `OpResult::Execute`,
`OpResult::FetchResource`, plus the WIT `reconcile` interface methods
that produce them (`plan-mutations`, `execute`, `fetch-resource`).
Runtime code currently only sees these arms in a validator test-helper
fallback, but the WIT surface itself also reserves the corresponding
operations.

**Before executing**: ask the user "is the reconcile interface reserved
for an in-flight mutation feature, or truly dead?" If dead, delete the
WIT operations, result records, and `op-result` arms together, then
regenerate. If reserved, leave them and add a one-line comment in WIT
naming the upcoming feature.

**Verify**: `just check`. Provider rebuilds clean.

---

### 8.2 Collapse cache re-skin types with WIT — **CONFIRM**

**Files**: `crates/host/src/cache/*.rs`,
`crates/host/src/runtime/wit_conversions.rs`,
plus all consumers of `SizeCache`/`BytesCache`/`StabilityCache`/
`ReadModeCache`/`EntryKindCache`.

**Background**: These cache types are 1:1 re-skins of their WIT
counterparts (`FileSize`, `ProjBytes`, `Stability`, `ReadMode`,
`EntryKind`). The `From` impls (~60 lines) translate variant-for-variant
with no semantic change. They're forced to track WIT changes anyway,
so the boundary buys nothing today.

**Before executing**: ask the user:
1. "Do we plan to version the L2 cache format independently of WIT?"
2. If yes, the cache types should stay — but they need real serde
   stability (explicit field order, `#[serde(tag, content)]`),
   which they don't have today. Document instead of merge.
3. If no, replace `cache::*Cache` with the WIT types, delete the
   `From` impls, and the cache module imports `wit_types` directly.

**Treat as a cache schema change.** `crates/host/src/cache/mod.rs`
declares `pub const SCHEMA_VERSION: u8 = 4;` and the file header
explicitly notes that on-disk records bump on encoding changes. The
cache types use `serde::{Serialize, Deserialize}` over `postcard`;
postcard encodes enum variants by index, and there is no guarantee
that `cache::SizeCache` and `wit_types::FileSize` (or the other
re-skin pairs) currently emit byte-identical postcard output — variant
order, struct field order, or `#[serde(rename)]` attributes can drift
silently.

**Required steps if this phase ever runs**:

1. Bump `SCHEMA_VERSION` in `crates/host/src/cache/mod.rs` (e.g. to 5)
   with a one-line comment naming the WIT collapse.
2. Capture postcard fixtures for every affected payload BEFORE the
   change (`LookupPayload`, `DirentsPayload`, `FilePayload`, `AttrPayload`).
3. Compare fixture bytes between old (`*Cache` types) and new
   (WIT types) encodings. If byte-identical, document the verification
   and merge. If not, the schema bump is load-bearing — old L2 records
   must be invalidated on first run, which the version field already
   handles.
4. Add a regression test that constructs each payload via the WIT
   types and round-trips through postcard.

Earlier wording claimed "the on-disk format is not changing in either
branch." That claim was unsafe — retracted. Treat this phase as a
cache schema migration and plan accordingly.

**Verify**: `just check` plus the fixture comparison from step 3
above. The L2 redb regression tests pass under the new version. Old
records (if any exist on a developer's disk) are dropped/rebuilt
without panicking — confirm by deleting `~/.cache` (or wherever the
provider cache lives) is NOT required, and that the version mismatch
path in `cache::l2` correctly invalidates stale entries.

---

## Final verification

After all phases:

- [ ] `just check` (fmt + clippy + test, host + providers).
- [ ] `just check-providers` (`--target wasm32-wasip2`).
- [ ] Manual: `just dev`, `just shell`, exercise FUSE smoke harness in
  `tests/smoke/`, confirm `ls`, `cat`, `find`, `grep`, `stat` all behave
  the same as on `main`.
- [ ] Manual: tail `/tmp/omnifs.log` inside the container during a
  browse session; verify the new span-based callout tracing emits one
  outer `callout` span and one inner executor span (`fetch`,
  `open_repo`, `open`, `read`) per callout, with each span producing
  `new` and `close` events from the subscriber's
  `FmtSpan::NEW | FmtSpan::CLOSE` config. The outer span carries
  `operation_id`/`callout_index`/`kind`. Inner spans carry the
  request-side fields at `new` and recorded fields (`status`,
  `response_headers`, `response_body_bytes`, `blob`, `tree_ref`,
  `error.kind`/`error.message`/`error.retryable`) at `close`.
  Elapsed time comes from the framework's per-span timing on `close`
  — there is no `elapsed_us` field anymore. Confirm redaction
  (`LogUrl`/`WitHeaders`) still strips credentials.
- [ ] Re-measure: `runtime/mod.rs` line count vs. baseline (~1840). Aim
  ≤ 600.
- [ ] Re-measure: total runtime LOC delta. Expect ~700–800 net deletion
  before Phase 8 confirmations.

## Wrap-up summary template

Post a structured summary per `~/.claude/rules/implementation-summary.md`:

- Headline: "Runtime callout pipeline cleanup landed; mod.rs from
  ~1840 → N lines."
- Summary table: commits landed, test-count delta per workspace,
  typecheck state.
- What landed: one bullet per phase.step in commit order.
- Delegation: none (single-agent work).
- What's next: any deferred items from Phase 8 confirmations.

---

## Quick reference: dependency graph

```
1.1 1.2 1.3 1.4 1.5 1.6   (independent)
              ↓
2.1 → 2.2 → 2.3 → 2.4 → 2.5 → 2.6 → 2.7 → 2.8 → 2.9
                                                  ↓
                                                 3.1
                                                  ↓
                                                 4.1
                                                  ↓
              5.0 (extract ProviderInstance — must precede file moves)
                                                  ↓
              5.1, 5.2, 5.4, 5.5  (sequential file moves;
                                   5.3 removed — folded into 7.1)
                                                  ↓
                                  6.1 → 6.2
                                                  ↓
                                  7.1   (Op + Validator co-location only;
                                         trait-based variant deferred)
                                                  ↓
                                  8.1 (CONFIRM)
                                  8.2 (CONFIRM — cache schema change)
```

## Deferred follow-ups (NOT in this PR)

Items called out during planning that are real wins but outside the
callout-pipeline scope, OR that need post-refactor data to design
properly. File as separate work items after this PR lands.

- **`SafeCacheKey` / `PathBackedCacheKey` newtype.** Both
  `crates/host/src/runtime/cloner.rs` and
  `crates/host/src/cache/blobs.rs` validate path-backed cache keys
  with their own logic. A newtype with a checked constructor would
  remove the duplication, but the design needs thought: blob has
  reserved `.tmp` / `.meta` extensions that git doesn't, so it's
  either one newtype with per-family constructors or two distinct
  newtypes. Decide after this PR.

- **`BrowseCache` boundary.** L2 + invalidation + activity all
  participate in browse caching but live as separate fields on
  `ProviderRuntime`. After Phase 3 (effects split) and Phase 5 (file
  reorg + `ProviderInstance` extraction), revisit whether grouping
  these under a single `BrowseCache` owner makes sense. Wait-and-see
  call — don't pre-design.

- **Per-call timeout for `HttpStack`.** Phase 6 ships two configured
  `HttpStack` instances (HTTP fetch = 30s, blob fetch = 120s). If a
  future change moves the timeout to a per-call parameter on
  `HttpStack::send`, collapse to one shared instance. Mechanical
  change, no design work needed.

- **JSON tracing layer.** Phase 2.6 reshapes the dev-mode plain-text
  log format. If any external tooling parses the old flat format, add
  a JSON layer behind a `--log-format=json` flag and migrate the
  tooling there. Scope is "wire up the layer", not redesign tracing.

- **Test seam for provider execution.** `ProviderInstance` is a concrete
  Wasmtime owner, not a stub seam by itself. If orchestration tests need
  to drive `run_op` without WASM, design a small `InstanceLike` trait or
  test adapter after 5.0 lands.

- **Trait-based `OpRequest`.** Revisit only if a future change forces
  typed callouts inside `coalesced`. The old code sketch was removed
  from this plan because it became stale after `ProviderInstance`.

## Stop conditions

Halt and report to the user if:

- Any step's `just check` fails for a reason that isn't a trivial
  follow-on edit.
- A behavior change appears in the FUSE smoke harness output.
- A tracing field name or structure changes outside Phases 2.6 and 2.7
  (the only phases that intentionally reshape tracing).
- Provider WASM components fail to load or initialize after a runtime
  change.
- The L2 cache fails to deserialize old records after a cache-touching
  change.
