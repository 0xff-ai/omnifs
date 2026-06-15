# File attributes for projected files

Status: implemented
Scope: `crates/omnifs-wit/wit/provider.wit`, host FUSE layer, SDK projection API, providers
Supersedes the relevant parts of: the earlier size-only model.

## Problem

A projected file in omnifs needs to communicate three core properties to the host:

- what the provider knows about its size,
- whether bytes are already inline or must be read later,
- how stable the bytes are expected to be.

It may also carry optional version evidence for mutable snapshots.

Earlier designs compressed these into:

- a single `option<u64>` size with a 256 MiB placeholder for "unknown",
- an implicit lazy read protocol where the host always called `read-file`,
- a `volatile: bool` flag that bundled cache bypass, direct I/O, zero TTLs, session-routed reads, and fh-aware getattr into one bit.

Each compression caused real bugs or left important shapes unexpressed:

- `tail -n` panicked on the 256 MiB placeholder.
- `tar c` archived 256 MiB worth of pad bytes for unsized files.
- Streamed but stable files, such as a large image layer that should be chunked and cacheable, had no expressible shape.
- Mutable but non-live files, such as `inspect.json`, were forced into the same bucket as live logs.

The redesign treats each property as a provider-declared fact or hint. The provider does not tell the host to refresh on open, bypass caches, or choose a FUSE flag. The host derives FUSE flags, cache placement, and invalidation behavior from the declared attributes.

## Bash-tool compatibility (the durable invariant)

omnifs paths must behave like real files for the standard Linux toolbox. The file-attribute model exists to make this true. Tools the design must not regress:

- **Read content**: `cat`, `head`, `tail` (incl. `-f`, `-n`, `-c`), `less`, `more`, `xxd`, `hexdump`, `od`, `file`.
- **Search and traversal**: `grep` (incl. `-r`), `rg`, `find` (incl. `-name`, `-size`, `-type`), `fd`.
- **Stat-based**: `ls` (incl. `-l`, `-h`), `du` (incl. `-sh`), `wc` (incl. `-l`, `-c`, `-m`), `stat`.
- **Copy and archive**: `cp`, `mv`, `tar` (`c`, `x`, `t`), `rsync`.
- **Compare and hash**: `diff`, `cmp`, `md5sum`, `sha256sum`, `b3sum`.
- **Inspection**: `jq`, `yq`, `xmllint`.
- **Editors**: `vim`, `neovim`, `nano`. Editors that mmap (some `code` configurations) are best-effort but should not break.

The deeper invariant the tool list is a witness for: **metadata is truthful or explicitly unavailable; bytes are consistent for the declared stability scope**. Tools that consult metadata before reading must not be misled into wrong decisions; tools that read must see consistent bytes for the duration the attributes promised.

There is no POSIX representation for "unknown length" or "known non-empty but unknown length." For `Size::NonZero` and `Size::Unknown`, streaming reads can be correct, but tools that make decisions entirely from `st_size` cannot be fully correct until the host learns an exact size. The contract must be honest about that boundary instead of replacing the old 256 MiB lie with a smaller one.

Stat-only or seek-from-end modes require `Size::Exact` to be reliable:

| Tool mode | Why `NonZero` / `Unknown` cannot be exact | Required shape |
|---|---|---|
| `tar c` | Tar writes the archive header size before reading file bytes. A header size of `1` or `0` cannot describe a larger payload. | `Size::Exact` before archive creation. |
| `wc -c` fast path | GNU `wc -c` can use `fstat` for regular files and avoid reading bytes. It will report the temporary `st_size`. | `Size::Exact`, or materialize first and rerun. |
| `tail -n`, `tail -c`, `less`, `lseek(SEEK_END)` | These modes seek from the reported end of file. An end offset of `1` or `0` is a lower bound, not the real end. | `Size::Exact`. |
| `du`, `find -size`, `rsync --size-only` | These modes are intentionally metadata-driven. They cannot discover bytes by streaming. | `Size::Exact` for size-sensitive decisions. |

## Attributes

```rust
pub struct FileAttrs {
    pub size: Size,
    pub stability: Stability,
    pub version: Option<VersionToken>,
}

pub struct VersionToken(pub String);

pub enum Size {
    /// Provider knows the exact byte length for this declared file
    /// observation or identity. `Exact(0)` is "known empty".
    Exact(u64),

    /// Provider knows the file is not empty, but does not know the exact
    /// length. This is a truthful provider hint, not an exact stat size.
    NonZero,

    /// Provider has no length information.
    Unknown,
}

pub enum Bytes {
    /// Bytes are carried with the projection. Size is derived from
    /// `bytes.len()`; providers must not supply a conflicting size.
    Inline(Vec<u8>),

    /// Bytes are not carried with the projection and must be read later.
    Deferred { read: ReadMode },
}

pub enum ReadMode {
    /// Provider can produce the whole payload in one read.
    Full,

    /// Provider can serve byte ranges through a file/session read path.
    Ranged,
}

pub enum Stability {
    /// Bytes for this file identity do not change.
    Immutable,

    /// Bytes may change between observations, but one observation should
    /// represent a consistent snapshot.
    Mutable,

    /// Bytes may change while being observed.
    Volatile,
}
```

These names are part of the public contract:

- `Size` describes what the provider knows about length.
- `Bytes` describes whether the projection already carries bytes.
- `ReadMode` describes the provider's deferred read capability.
- `Stability` describes the provider's claim about byte stability.

The attributes are deliberately declarative. They are not host instructions. For example, `Stability::Mutable` does not mean "refetch on open"; it means permanent content caching is not safe unless the host has invalidation or version proof.

## Size semantics

`Size::Exact(n)` means the provider knows the exact byte length for the declared file observation or identity.

For `Stability::Immutable`, exactness can be treated as exact for the file identity. For `Stability::Mutable`, exactness is scoped to the projected observation unless a stronger version identity says otherwise. For `Stability::Volatile`, exactness is only meaningful if the live source itself exposes a stable length for the observation.

`Size::NonZero` means the provider knows the file has at least one byte, but does not know the exact length. This is weaker and more truthful than an estimate. It is useful because many upstream systems can cheaply distinguish empty from non-empty without computing a byte length.

`Size::Unknown` means the provider has no length information.

FUSE still requires the host to return a numeric `st_size` for regular files. `Size::NonZero` and `Size::Unknown` are not directly representable as honest POSIX file lengths. The host must choose a compatibility policy for stat reporting, direct I/O, and post-read size learning, but it must not treat `NonZero` as an exact byte bound.

## Bytes semantics

`Bytes::Inline(bytes)` means the projection already contains the file bytes. Inline files derive their exact size from `bytes.len()`. The SDK and host boundaries reject `Bytes::Inline` unless `size == Size::Exact(bytes.len() as u64)`.

Inline bytes are capped by the SDK's eager byte limit, currently `MAX_PROJECTED_BYTES = 64 KiB`. This cap is part of the provider contract because inline bytes ride inside listings and lookup responses. Larger payloads must use `Bytes::Deferred { read: ReadMode::Full }` or `Bytes::Deferred { read: ReadMode::Ranged }`.

The host also enforces a response-level eager-byte cap across all inline entries in one terminal response (direct entries plus any inline content in `effects.fs` entries). The implementation cap is `MAX_EAGER_RESPONSE_BYTES = 512 KiB`. The boundary rejects responses that exceed the aggregate cap; it does not silently promote excess inline files to deferred reads, because that would require inventing a handler path or changing provider-declared byte semantics.

`Bytes::Deferred { read: ReadMode::Full }` means the provider can produce the whole payload when the host asks to read the file.

`Bytes::Deferred { read: ReadMode::Ranged }` means the provider can serve byte ranges, normally through the `open-file` / `read-chunk` / `close-file` session path. Ranged reads must expose EOF explicitly or by a protocol result that is unambiguous at the WIT boundary; otherwise the host cannot learn a complete size from ranged reads.

For `Size::Exact(n)`, deferred reads are checked against `n`. A full read that returns a different length is a provider contract error. A ranged read that returns bytes beyond `n`, or fails to return bytes inside `0..n`, is also a provider contract error. The host surfaces these contract violations as hard read failures, normally `EIO`, and does not deliver partial bytes as if they were valid file content. For `Size::NonZero` and `Size::Unknown`, a complete full read learns the exact size. A complete ranged read can learn the exact size only when the ranged protocol proves EOF.

For `Size::Exact(0)`, the host can serve EOF without calling a deferred handler. Providers should normally use inline empty bytes for small known-empty files, but deferred exact-empty declarations remain valid.

`ReadMode` is provider capability, not cache policy. The host may still choose direct I/O, page cache behavior, in-memory overlay or durable view cache placement, or post-read invalidation based on `Size` and `Stability`.

## Stability semantics

`Stability::Immutable` means bytes for this file identity do not change. Once materialized, the host can cache content indefinitely subject to capacity and explicit invalidation of the identity.

`Stability::Mutable` means bytes may change between observations, but each observation should be a consistent snapshot. For inline bytes and full reads, the observation is the projected entry or the full read response. For ranged reads, one open file handle is one observation, so chunks served through that handle must belong to the same snapshot. The host should not assume permanent content caching is safe unless invalidation or version identity proves it. This covers normal dynamic files such as status, metadata, API response projections, and small JSON/text files that may be regenerated.

`Stability::Volatile` means bytes may change while being observed. For ranged volatile reads, each `read-chunk` can observe a newer source state than the previous chunk. This covers live logs, tail-like files, metrics streams, and other moving targets. The host must not serve these from normal whole-file caches.

`version` is optional provider evidence for one observed file identity. It can be an ETag, commit SHA, API update timestamp, content digest, or upstream revision. It is a fact about the observation, not an instruction to cache. When absent, `Stability::Mutable` defaults to observation-only caching unless an invalidation channel covers the path.

Version tokens are meaningful cache-key material only for `Stability::Mutable`. For `Stability::Immutable`, a version token is redundant metadata and must not create a second cache identity for the same immutable file. For `Stability::Volatile`, a version token is observation-tagging metadata only and must not make volatile bytes durably cacheable. Tokens are non-empty opaque UTF-8 strings, capped at 256 bytes, compared by byte equality with no normalization.

## Host-derived policy

The provider declares attributes; the host derives FUSE and cache behavior from them.

| Attributes | `st_size` before read | Open/read policy | Content cache policy | Post-read size learning |
|---|---:|---|---|---|
| `Size::Exact(n)` | `n` | Normal reads unless `Stability::Volatile` forces the ranged live path. | Derived from `Stability`. | Already exact. Inline and deferred reads validate against `n`. |
| `Size::NonZero` | `1` | Materialize full-deferred files on open before the first read; use direct I/O while non-exact. | Derived from `Stability`. | Learn real size only when the read shape proves a complete observation. |
| `Size::Unknown` | `1` | Materialize full-deferred files on open before the first read; use direct I/O while non-exact or until the file is known to be live. | Derived from `Stability`. | Learn real size only when the read shape proves a complete observation. |

`Size::NonZero` reports `st_size = 1` before materialization. This preserves a truthful non-empty signal for stat-only tools without pretending to know the length. The host must still treat that size as a lower-bound hint only: it is never used to clamp reads, allocate final buffers, or decide EOF.

`Size::Unknown` reports `st_size = 1` before materialization. This is a compatibility sentinel, not a provider claim that the file is non-empty. Linux tools such as `cat` can skip reads for zero-sized regular files even when the host asks for direct I/O, so the host uses the smallest non-zero stat value to force materialization. A complete read can still learn and publish `Size::Exact(0)`.

For both `NonZero` and `Unknown`, direct I/O alone is not enough to make the reported stat size invisible. Linux can still make the old `st_size` observable to the reader. For full-deferred files, the host materializes content during `open`, publishes the learned exact size before the post-open `getattr`, and serves the first read from the open file handle. Cached non-exact dirents must not downgrade that learned exact size when they describe the same stability, byte mode, and version identity.

Size learning is valid only when the host has a complete observation:

- `Deferred { read: Full }` learns the exact size from the returned byte length, subject to stability cache rules.
- `Deferred { read: Ranged }` learns the exact size only when the ranged protocol proves EOF for the observation.
- `Stability::Volatile` does not publish a durable learned size, because later chunks or opens can observe a different source state.

`Size::Exact(n) + Stability::Volatile` still reports `st_size = n`, but reads use the live ranged path. The exact size is metadata about the listing or lookup observation that produced it, not permission to serve bytes from a whole-file cache or clamp future live reads without checking the source. Because volatile sizes can drift and are not learned durably from reads, the host treats volatile attrs as file-handle-aware or short-TTL metadata; a fresh listing or lookup is required to update `n`.

For `Stability::Mutable`, each listing, lookup, open, or full-read observation supersedes prior size and bytes for that path unless the prior and current observations share the same version token. If a later observation reports `Size::Exact(m)` where an earlier observation reported `Size::Exact(n)`, the host invalidates cached kernel attrs and publishes the newer observation according to the same cache-proof rules.

The host-derived FUSE policy is deliberately host-owned:

| Stability / size condition | FUSE/cache behavior |
|---|---|
| `Immutable + Exact` | Normal attribute TTLs and content caches are safe by identity. |
| `Immutable + NonZero/Unknown` | Materialize full-deferred files on open, use direct I/O while non-exact, and retain learned exact size across stale non-exact dirent refreshes. |
| `Mutable` without version or invalidation proof | Keep content and learned size within the current observation only. Use short attribute TTLs and do not reuse whole-file content across opens. |
| `Mutable` with version or invalidation proof | Cache by version identity and invalidate path aliases through provider events. |
| `Volatile` | Use the ranged live path, direct I/O, short or zero attribute TTLs, and file-handle-aware attributes where the kernel path requires them. Do not use durable whole-file caches. |

Cache placement follows `Stability`:

| Stability | Host policy |
|---|---|
| `Immutable` | File content and learned size may be cached by identity until capacity eviction or explicit invalidation. |
| `Mutable` | File content and learned size may be cached only when tied to a version proof or invalidation regime. Without that proof, cache only within the current observation. |
| `Volatile` | Do not place whole-file content, ranged chunks, or learned size in durable file caches. Per-handle buffers are allowed only as implementation details of one live read path. |

## Mutable proof model

`Stability::Mutable` is intentionally weaker than a host instruction. It says the bytes may change between observations, while each observation should be consistent. The host needs proof before it treats mutable content as durably cacheable.

There are three viable proof mechanisms:

| Mechanism | Shape | Pros | Cons |
|---|---|---|---|
| Explicit invalidation | Provider returns `effects { invalidations: [...] }` carrying `object(logical-id)` or `listing(path-or-prefix)` entries when upstream state changes. | Fits the existing host cache model and works for event-driven providers. | Requires reliable provider events; polling gaps can serve stale content. |
| Version identity | Provider attaches `version` to the file observation, such as an ETag, commit SHA, API update timestamp, or content digest. | Makes cache keys precise and lets mutable files become cacheable snapshots. | Requires providers to define token scope consistently for each path. |
| Observation-only cache | Host caches mutable content only inside one read/open observation. | Safe default with no extra protocol. | Gives up cross-open cache reuse for mutable files. |

The default policy should be observation-only. Providers can opt into stronger caching by supplying either invalidation proof or version identity. When both exist, version identity should key cached content, while invalidation removes stale path aliases and directory metadata.

Versioned mutable content is keyed by `(provider-id, file-identity, version-token)`. `file-identity` is the provider-relative file path under that provider. The token is not globally unique by itself; two paths may legitimately share the same ETag or digest.

Invalidation is delivered through the `invalidations` field in the provider return's `effects` record. A provider that declares `Stability::Mutable` but has no event source covering that path has not supplied invalidation proof; it remains observation-only unless it supplies a version token. This keeps `Mutable` as a provider fact and keeps refresh timing as host policy.

## Legal combinations

One structural rule constrains the matrix:

- **`Stability::Volatile` requires `Bytes::Deferred { read: ReadMode::Ranged }`.**

Inline bytes and full deferred reads produce a whole-file observation. They cannot model bytes that change during the same observation. Ranged/session reads are the only deferred shape that can represent a live file without stitching together unrelated whole-file snapshots.

| Bytes | `Stability::Immutable` | `Stability::Mutable` | `Stability::Volatile` |
|---|---|---|---|
| `Inline(bytes)` | Valid. The projection carries exact bytes and size is `bytes.len()`. The host can cache the materialized content for the file identity. | Valid. The projection carries a consistent snapshot. The host must not treat it as permanently cacheable without invalidation or version proof. | Invalid. Inline bytes cannot change while being observed. |
| `Deferred { read: Full }` | Valid. The provider can fetch the whole file on read. After a successful read, the host can cache content for the file identity. | Valid. A whole-file read produces one consistent observation. The host must not treat the content as permanently cacheable without invalidation or version proof. | Invalid. Whole-file reads cannot safely model bytes changing during observation. |
| `Deferred { read: Ranged }` | Valid. Large stable files can be served in chunks and cached by identity. | Valid. Large mutable files can be served as ranged consistent observations. Permanent caching still requires invalidation or version proof. | Valid. This is the required shape for live, tail-like, or otherwise moving files. |

Size compatibility rules:

| Size | Meaning | Notes |
|---|---|---|
| `Exact(n)` | Provider knows the exact byte length for the declared file observation or identity. | For inline bytes, derive this from `bytes.len()`. For mutable files, exactness is scoped to the observation unless version identity says otherwise. |
| `NonZero` | Provider knows the file is not empty but does not know the length. | Useful hint, but not an exact `st_size`. The host must not use it as a read bound. |
| `Unknown` | Provider has no length information. | The host should avoid treating stat size as a read bound. |

Hard validation rules:

- `Stability::Volatile` requires `Bytes::Deferred { read: ReadMode::Ranged }`.
- `Bytes::Inline(bytes)` requires `Size::Exact(bytes.len() as u64)`.
- `Bytes::Inline(bytes)` rejects `Size::NonZero` and `Size::Unknown`.
- `Bytes::Inline(bytes)` is rejected when `bytes.len() > MAX_PROJECTED_BYTES`.
- Inline bytes are also rejected when the terminal response exceeds the aggregate eager-byte cap.
- `Size::Exact(n)` mismatch with inline bytes is rejected after WIT decode. If the provider detects it first, it returns `op-result.err(provider-error)`; if the host detects it, the user-facing operation fails rather than publishing the entry.
- A deferred read that contradicts `Size::Exact(n)` is a provider contract error.
- `version` rejects empty strings and strings longer than 256 bytes.
- These rules apply uniformly to directory entries, lookup targets, lookup siblings, and preloaded entries returned from byte-producing handlers (via the `effects { fs, canonical }` channels).

## Declaring files

The provider declares files via the imperative registration API. Inline files carry bytes inside `Bytes::Inline`; deferred files are served by a handler registered with `r.file(t).handler(h)`.

```rust
// Inline: bytes carried with the projection, no separate file handler needed.
// r.file("/_README.md").handler(readme_file)?;  -- or inline via projection builder:
FileProjection::inline(README.into()).immutable().build()

// Deferred full read: handler called on read-file.
FileProjection::deferred(Size::Unknown).full().mutable().build()

// Deferred full read with known size and version:
FileProjection::deferred(Size::Exact(8)).full().mutable().version(state_revision).build()

// Deferred ranged read: handler called on open-file / read-chunk.
FileProjection::deferred(Size::Exact(layer_size)).ranged().immutable().build()

// Deferred ranged volatile: live-log shape.
FileProjection::deferred(Size::NonZero).ranged().volatile().build()
```

For byte-producing handlers:

| Bytes | Bytes from | Handler returns |
|---|---|---|
| `Inline(bytes)` | Projection builder | No separate file handler needed |
| `Deferred { read: Full }` | registered `r.file(t).handler(h)` | `FileProjection` with full deferred bytes |
| `Deferred { read: Ranged }` | registered `r.file(t).handler(h)` | `FileProjection` with ranged deferred bytes; `read-chunk` results carry `{ content, eof }` |

The SDK and host validate attrs at the response boundary and at read/open time. A `Deferred { read: Full }` path whose `read-file` handler returns ranged content is a provider contract error. A `Deferred { read: Ranged }` path whose `open-file` handler returns whole bytes is a provider contract error. Inline-declared paths do not need a registered file handler; if a handler is registered anyway, the inline payload can still satisfy cached reads for that observation.

Validation phase:

- Registry build validates route conflicts before the provider serves requests.
- Runtime validates dynamic projection entries returned by handlers, including inline size consistency, inline byte caps, illegal volatile combinations, version tokens, and aggregate eager-byte caps.
- `read-file` validates complete content against returned attrs and rejects streams or ranged content on the full-read path.
- `open-file` validates ranged attrs and rejects whole-byte content on the ranged path.
- First-use runtime errors are expected for dynamic handler/attrs pairing violations that cannot be proven from route shape alone.

## WIT shape

The SDK API can expose ergonomic Rust enums while WIT stays transport-shaped.

```wit
type version-token = string;
type file-handle = u64;
type tree-ref = u64;

variant file-size {
    exact(u64),
    non-zero,
    unknown,
}

enum read-mode {
    full,
    ranged,
}

// Byte availability is kept on a separate byte-source variant.
variant byte-source {
    inline(list<u8>),
    canonical,
    blob(blob-id),
    deferred(read-mode),
}

enum stability {
    immutable,
    mutable,
    volatile,
}

// file-attrs carries metadata only; byte availability lives in file-out.bytes.
record file-attrs {
    size: file-size,
    stability: stability,
    version-token: option<version-token>,
}

record file-out {
    content-type: option<string>,
    attrs: file-attrs,
    bytes: byte-source,
}

variant entry-kind {
    directory,
    file(file-out),
}

record dir-entry {
    name: string,
    kind: entry-kind,
    id: option<logical-id>,
}

record read-chunk-result {
    content: list<u8>,
    eof: bool,
}

// Completed full-read result.
record read-file-result {
    content-type: option<string>,
    attrs: file-attrs,
    bytes: byte-source,
}

// Opened ranged-read handle.
record open-file-result {
    handle: file-handle,
    attrs: file-attrs,
}
```

File attributes are carried by the `file(file-out)` entry-kind payload. Directories do not have file attributes, and a regular file without attrs is a protocol error rather than an optional case. Every response surface that carries entries, including `dir-listing.entries`, `lookup-entry.target`, and `lookup-entry.siblings`, carries the same shape.

Subtree handoffs remain a separate terminal shape, such as `list-result.subtree(tree-ref)`. They do not appear inside `dir-entry.kind`; `entry-kind` only describes entries materialized in a directory listing or lookup response.

Inline bytes live inside `byte-source.inline`. The SDK and host validate after WIT decode that inline bytes imply an exact size equal to `content.len()`, then the host caches the bytes at the response boundary.

Deferred full files map to the `read-file` operation. Deferred ranged files map to the `open-file` / `read-chunk` / `close-file` operation family. The ranged request shape is explicit-offset: `open-file(path) -> file-handle`, `read-chunk(handle, offset, len) -> read-chunk-result`, and `close-file(handle)`.

`read-chunk` returns `read-chunk-result { content, eof }`. For `Mutable + Ranged`, `eof = true` means the end of the snapshot for that open handle, so the host may learn an observation-scoped size. Re-reading the same `(offset, len)` on one mutable handle must return bytes from the same snapshot. For `Volatile + Ranged`, `eof = true` only terminates the current live read and never publishes a durable learned size.

Additional paths learned while serving a request travel through the effects record (`effects { canonical, fs, invalidations }`). The `fs` field carries `fs-write` entries for paths the host should install into the view cache; the `canonical` field carries `canonical-store` entries for object anchors. There are no separate sibling-file or preload fields on the terminal result.

## Bash-tool acceptance matrix

These cases are the minimum behavior expected from the supported Linux toolbox.

| Shape | Tool scenario | Expected behavior |
|---|---|---|
| `Exact + Inline + Immutable` | `ls -l`, `cat`, `tar c`, `rsync --size-only` | `st_size` equals `bytes.len()`. Reads serve the inline bytes. Archive/copy tools see the real size and complete without padding. |
| `Exact + Deferred + Immutable` | `cp`, `sha256sum`, `tar c` | Host reads deferred bytes, validates/read-clamps against exact size, and may cache content by identity. |
| `NonZero + Deferred + Immutable` | `find -size +0`, `cat`, later `stat` | Initial stat reports size `1`, so non-empty-sensitive tools do not skip it. Opening the file materializes the full content and publishes the exact size before the first read. Later stat reports exact size. |
| `Unknown + Deferred + Immutable` | `cat`, `head`, later `stat` | Initial stat reports size `1` as a compatibility sentinel. Opening the file materializes the full content and publishes exact size before the first read, including exact zero. |
| `Exact + Inline + Mutable` | `cat` twice across observations | Each projected inline payload is a consistent snapshot. Host does not reuse it permanently unless invalidation or version identity proves it is still current. |
| `Unknown + Deferred + Mutable` | `cat`, then later `cat` | Each complete read produces a consistent observation. Learned size is observation-scoped unless version identity or invalidation proof allows durable reuse. |
| `NonZero + Deferred + Volatile` | `tail -f`, repeated `read` on one handle | Initial stat reports size `1`. Host uses the ranged live path, avoids whole-file caches, and does not publish a durable learned size from partial live reads. |
| `Unknown + Deferred + Volatile` | `tail -f`, `head`, `less` | Initial stat reports size `1` as a compatibility sentinel. Host uses the ranged live path. Tools that stream reads should see current bytes without stale whole-file caching. |

For `NonZero` and `Unknown`, the matrix intentionally excludes `tar c`, `wc -c`, `tail -n`, `tail -c`, and size-only traversal modes as guaranteed scenarios. Those tools can be correct after materialization promotes the inode to `Size::Exact`, but they cannot be promised before exact size is known.

## Why the 256 MiB placeholder is gone

Earlier versions of omnifs reported a 256 MiB placeholder for unsized files. That single value tried to be both a read-termination upper bound, which must be larger than any payload to avoid truncation, and a userspace size report, which must be small enough to not lie. 256 MiB was the compromise; it was unsound in both directions. `tail -n` would `lseek(SEEK_END)` to 256 MiB and panic walking back through unallocated space; `du -sh` over a directory of projected files reported terabytes; some real downloads exceeded the bound and silently truncated.

The replacement is conservative but not magic. `Size::NonZero` and `Size::Unknown` make `du` and size-only tools under-report until the file is materialized. That is still preferable to a large fabricated length because it makes the absence of size knowledge explicit while preserving normal read behavior for `cat`-style tools.

The new model decouples provider knowledge from host FUSE policy. `Size::Exact` is an exact length. `Size::NonZero` is a truthful lower-bound hint. `Size::Unknown` is explicit absence of length information. The host then chooses how to expose stat size and direct I/O without pretending a compatibility sentinel is an exact file length.
