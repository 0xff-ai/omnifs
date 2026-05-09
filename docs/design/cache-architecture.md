# Cache architecture

Status: accepted
Scope: host browse cache, blob cache, archive tree materialization, and the cache invalidation boundary between providers and FUSE

## Context

omnifs has several host-owned caches with different identities and
different visibility rules. They should not be collapsed into one generic
cache because each tier answers a different question.

The browse cache answers FUSE questions about projected paths: lookups,
attributes, directory entries, and file bytes. The blob cache stores
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
blobs/<cache-key>           # fetched blob body
blobs/.meta/<cache-key>.json
blobs/.tmp/                 # fetch staging
archives/<view-dir>         # extracted archive tree
```

## Decisions

### D1. Cache identity stays local to each cache kind

Browse cache identity is `cache::Key { path, kind }`, where `path` is the
logical provider path and `kind` is `Lookup`, `Attr`, `Dirents`, or
`File`. L0 and L2 use the same logical key so promotion from L2 into L0
does not translate identity.

Blob cache identity is the provider-supplied `cache-key`. Runtime
`blob-id` values are handles assigned by the current host process; they
are not durable and must not appear in stable cache keys.

Archive cache identity is the complete semantic archive view:
`(blob cache-key, archive format, strip-prefix)`. The archive cache never
keys on `blob-id`, because the same on-disk blob is assigned new ids after
a host restart. Different views of the same blob must remain independent
and stable.

### D2. L0 is a small per-mount memory overlay

`cache::l0::Cache` is owned by `FuseFs` and keyed by mount name plus
`cache::Key`. It is a byte-weighted moka cache with a 32 MiB maximum
weight per provider instance.

L0 intentionally skips records whose payload is larger than 256 KiB.
That keeps large file responses out of the hot memory tier while still
allowing small lookup, attribute, directory, and file records to avoid
provider round trips.

Invalidation reaches L0 from the runtime after provider event outcomes.
`drain_and_evict_pending` removes exact paths and segment-bounded prefixes
from L0, and it also clears matching `path_to_inode` dedup entries. It
does not delete live inode records, because FUSE handles that are already
open must remain resolvable.

### D3. L2 is the durable browse cache

`cache::l2::Cache` stores browse records in a per-provider redb database
at `browse.redb`. It has three tables: `metadata`, `content`, and `bulk`.
Lookup, attribute, and directory records live in `metadata`; file records
live in `content` until their serialized payload crosses the 64 KiB bulk
threshold, then move to `bulk`.

The L2 wire key is `{kind-char}:{path}`. Prefix deletion scans each record
kind and then applies segment-bounded path matching, so invalidating
`foo` removes `foo` and `foo/bar` without removing `foobar`.

Malformed records and unknown schema versions are treated as cache
misses. The cache should cause a provider refresh, not make FUSE fail
because an old or corrupt browse record was present.

### D4. Directory entries can imply lookup results only when exhaustive

Cached directory entries carry an `exhaustive` bit. FUSE may answer
positive and negative child lookup results from cached parent dirents only
when the parent listing is exhaustive.

Non-exhaustive dirents exist for preloaded sibling records and projected
structure. They may warm later lookups, but they cannot be treated as the
complete listing for `opendir` or negative lookup answers.

### D5. Blob bodies are cached on disk and rehydrated by cache key

`BlobCache` is scoped to one provider runtime and stores each response
body at `blobs/<cache-key>`. Metadata is stored beside it in
`blobs/.meta/<cache-key>.json`. The sidecar is required for rehydration;
blob files with missing or malformed sidecars are skipped on startup.

`fetch-blob` coalesces concurrent fetches for the same key with an
in-process key lock. It streams the response body into `blobs/.tmp/`,
enforces the configured fetch cap while streaming, writes the metadata
sidecar via a temporary-file rename, then publishes the body into its
final path. A visible body plus valid sidecar is the committed cache
entry.

`read-blob` reads from the cached body by runtime `blob-id`, offset, and
optional length. The configured read cap is enforced on the number of
bytes returned by that single call, including read-to-EOF requests.

### D6. Archive trees are materialized by semantic view and published by rename

`ArchiveExecutor` maps `open-archive(blob, format, strip-prefix)` to an
`ExtractKey` made from the blob record's stable `cache-key`, the archive
format, and the normalized strip prefix. `ExtractKey::dir_name` produces a
stable archive cache directory name without embedding the raw cache key in
the archive path.

`TreeMaterializer<ExtractKey>` coalesces concurrent materializations of
the same key. It writes the extractor output into a sibling temporary
directory, publishes the completed directory with `publish_dir_by_rename`,
registers the published path in `TreeRegistry`, and returns a runtime
`tree-ref`.

The rename gives atomic visibility on one filesystem: FUSE sees either
the previous completed tree or the newly completed tree. The helper does
not claim crash durability because it does not fsync the tree and parent
directory.

### D7. Tree refs are handles, not cache keys

`tree-ref` values are runtime-local handles in `TreeRegistry`. They are
the traversal mechanism used by FUSE after a provider returns a subtree,
but they are not stable storage identity.

The stable identity for an extracted archive tree is the archive view key
and its derived directory under `archives/`. On restart, the materializer
can register an existing published directory and return a fresh `tree-ref`
for the same semantic view.

## Failure behavior

Cache failures should degrade toward misses where that is safe. A failed
L2 open disables durable browse caching for that provider runtime and logs
the error. A corrupt L2 record is ignored. A missing blob sidecar prevents
rehydration for that blob, but a future `fetch-blob` for the same key can
populate it again.

Publication failures are not cache misses. If the host cannot publish a
blob body or extracted tree after a tool run, the operation returns an
internal error because the host cannot safely expose the requested data.

## Invariants

- L0 and L2 browse cache keys are identical logical keys.
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
