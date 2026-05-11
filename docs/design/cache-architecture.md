# Cache architecture

Status: accepted
Scope: host browse cache, blob cache, archive tree materialization, and the cache invalidation boundary between providers and FUSE

## Context

omnifs has several host-owned caches with different identities and
different visibility rules. They should not be collapsed into one generic
cache because each tier answers a different question.

The browse cache answers FUSE questions about projected paths: lookups,
attributes, directory entries, and file bytes. File bytes in the browse cache are
durable only for immutable files or versioned mutable files. The blob cache stores
provider-fetched HTTP response bodies on disk so large payloads do not
cross the WIT boundary. The archive tree cache stores materialized views
of cached blobs and returns runtime-local `tree-ref` handles that FUSE can
traverse through a real backing directory.

The provider instance cache root is:

```text
<cache-dir>/providers/<mount>/
```

The current on-disk layout under that root is:

```text
browse.redb                 # L2 browse cache
blobs/<cache-key>           # fetched blob body; key is a safe relative path
blobs/.meta/<cache-key>.json # matching JSON metadata under the same key path
blobs/.tmp/                 # fetch staging
archives/<format>-<hash>    # extracted archive tree for a semantic view
```

## Decisions

### D1. Cache identity stays local to each cache kind

Browse cache identity is `cache::Key { path, kind, aux }`, where `path`
is the logical provider path and `kind` is `Lookup`, `Attr`, `Dirents`,
or `File`. `aux` is optional key material for durable file content, used
today for mutable files as `version:<token>`. L0 and L2 use the same
logical key, including `aux`, so promotion from L2 into L0 does not
translate identity.

Blob cache identity is the provider-supplied `cache-key`. Runtime
`blob-id` values are handles assigned by the current host process; they
are not durable and must not appear in stable cache keys.

Archive cache identity is the complete semantic archive view:
`(blob cache-key, archive format, strip-prefix)`. The archive cache never
keys on `blob-id`, because the same on-disk blob is assigned new ids after
a host restart. Different views of the same blob must remain independent
and stable.

### D2. L0 is a small per-mount memory overlay

`FuseFs` owns a `DashMap<String, cache::l0::Cache>` and creates one L0
cache per mount name. Each per-mount cache is keyed by `cache::Key` and
is a byte-weighted moka cache with a 32 MiB maximum weight per provider
instance.

L0 intentionally skips records whose payload is larger than 256 KiB.
That keeps large file responses out of the hot memory tier while still
allowing small lookup, attribute, directory, and file records to avoid
provider round trips. Durable `RecordKind::File` entries are written only
when `FileAttrsCache::durable_cache_aux()` is `Some`: immutable files use
no aux, mutable files use `version:<token>`, and volatile or unversioned
mutable files are handled through non-durable read paths and handle cache.

Invalidation starts in the runtime at provider event outcomes. Exact path
invalidation removes matching L2 rows and path->inode entries immediately.
Prefix invalidation removes matching L2 rows and kernel directory entries and
records pending paths for later FUSE cleanup. `drain_and_evict_pending`
processes queued exact/prefix invalidations from runtime, removes matching
`path_to_inode` entries, and evicts matching L0 rows when the per-mount L0
cache is present.

### D3. L2 is the durable browse cache

`cache::l2::Cache` stores browse records in a per-provider redb database
at `browse.redb`. It has three tables: `metadata`, `content`, and `bulk`.
Lookup, attribute, and directory records live in `metadata`; file records
live in `content` while their payload is smaller than 64 KiB and move to
`bulk` when the payload length is at least 64 KiB.

The L2 stored key is `{kind-char}:{path}` plus an optional
`\u{1f}{hex(aux)}` suffix. The kind chars are `L`, `A`, `D`, and `F` for
lookup, attribute, dirents, and file records. Exact deletion scans one
kind-prefixed path range per record kind across all three tables and then
matches the decoded stored path exactly. Prefix deletion uses the same
range shape and applies segment-bounded path matching, so invalidating
`foo` removes `foo` and `foo/bar` without removing `foobar`.

Malformed records and unknown schema versions are treated as cache
misses.
Response-boundary limits used by browse payload validation:

- `MAX_PROJECTED_BYTES` caps eager projection payloads at 64 KiB.
- `MAX_EAGER_RESPONSE_BYTES` caps total inline terminal payloads at
  512 KiB.
- `MAX_VERSION_TOKEN_BYTES` caps version-token length at 256 bytes.

### D4. Directory entries can imply lookup results only when exhaustive

Cached directory entries carry an `exhaustive` bit. FUSE may answer
positive and negative child lookup results from cached parent dirents only
when the parent listing is exhaustive.

Non-exhaustive dirents exist for preloaded sibling records and projected
structure. They may warm later lookups, but they cannot be treated as the
complete listing for `opendir` or negative lookup answers.

### D5. Blob bodies are cached on disk and rehydrated by cache key

`BlobCache` is scoped to one provider runtime and stores each response
body at `blobs/<cache-key>`. The key is a safe relative path: it cannot
be empty, absolute, contain NUL, use `.` or `..` components, or collide
with the reserved `.tmp` and `.meta` directories. Metadata is stored in
`blobs/.meta/<cache-key>.json` under the same relative key path. That
metadata file is required for rehydration; blob files with missing or
malformed metadata are skipped on startup.

`fetch-blob` coalesces concurrent fetches for the same key with an
in-process key lock. It streams the response body into `blobs/.tmp/`,
enforces the configured fetch cap while streaming, writes metadata via a
temporary-file rename, then publishes the body into its final path. The
default fetch cap is 1 GiB and the default `read-blob` cap is 16 MiB,
unless the provider instance overrides `max_fetch_blob_bytes` or
`max_read_blob_bytes`. A visible body plus valid metadata is the
committed cache entry.

`read-blob` reads from the cached body by runtime `blob-id`, offset, and
optional length. The configured read cap is enforced on the number of
bytes returned by that single call, including read-to-EOF requests.

FUSE also keeps a per-open-file-handle in-memory buffer cache for file reads
(`file_cache`) that is cleared on `release`; this is an ephemeral runtime
cache and is not part of L0/L2 browse-keyed cache semantics.

### D6. Archive trees are materialized by semantic view and published by rename

`ArchiveExecutor` maps `open-archive(blob, format, strip-prefix)` to an
`ExtractKey` made from the blob record's stable `cache-key`, the archive
format, and the raw non-empty strip-prefix string. `ExtractKey::dir_name` produces a
stable archive cache directory name as `<format-component>-<sha256-prefix>`,
where the hash covers the blob cache key and the raw strip-prefix marker.
Empty strip prefixes are treated as absent.

Archive materialization is bounded by extractor defaults:

- `max_entries`: 50,000
- `max_file_size`: 256 MiB
- `max_total_bytes`: 1 GiB
- `max_path_depth`: 64
- `max_path_len`: 4,096
- `fuel`: 5,000,000,000
- `max_memory_bytes`: 256 MiB

`TreeMaterializer<ExtractKey>` coalesces concurrent materializations of
the same key. It writes the extractor output into a sibling temporary
directory, publishes the completed directory with `publish_dir_by_rename`,
registers the published path in `TreeRefs`, and returns a runtime
`tree-ref`.

The rename gives atomic visibility on one filesystem: FUSE sees an
already completed cached directory or a freshly published completed
directory, never the temporary extraction tree. The helper does not claim
crash durability because it does not fsync the tree and parent directory.

### D7. Tree refs are handles, not cache keys

`tree-ref` values are runtime-local handles in `TreeRefs`. They are
the traversal mechanism used by FUSE after a provider returns a subtree,
but they are not stable storage identity.

The stable identity for an extracted archive tree is the archive view key
and its derived directory under `archives/`. On restart, the materializer
can register an existing published directory and return a fresh `tree-ref`
for the same semantic view.

## Failure behavior

L2 open failures disable durable browse caching for that provider runtime and
log the error. L2 get failures and corrupt records are treated as misses.
L2 put/delete failures are non-fatal and are debug-logged; they may leave stale
durable rows behind.

Missing blob metadata blocks blob rehydration for that key until a future
`fetch-blob` repopulates it.

`open-archive` returns `NotFound` when the blob id is missing.
Archive extract, archive prepare, archive publish, and blob publish failures are
internal or invalid-input based on extractor diagnostics; publication failures use
internal errors when the host cannot safely expose the requested data.

## Invariants

- L0 and L2 browse cache keys are identical logical keys, including
  optional auxiliary key material.
- Runtime-local `blob-id` values never participate in durable cache
  identity.
- Archive tree identity includes the full `(cache-key, format,
  strip-prefix)` view.
- FUSE only trusts cached parent dirents for negative child lookups when
  the dirents record is exhaustive.
- Provider invalidation uses exact paths and segment-bounded prefixes.
- Published archive trees are real directories, not symlinks.
- Temporary blob files and temporary materialization directories are
  implementation details and are swept best-effort on startup.

## Non-goals

- A single shared cache trait for browse records, blobs, archive trees,
  and git clones.
- Persisting runtime handle values such as `blob-id` or `tree-ref`.
- Cross-process cache locking or multi-writer cache coordination.
- Crash-durable publication semantics stronger than same-filesystem
  rename visibility.
- Mutation cache semantics. The current cache model describes read-side
  projection, blob fetches, and materialized read-only trees.
