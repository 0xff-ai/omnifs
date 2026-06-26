# File attributes

Status: current-architecture
Scope: why projected file attrs are shaped as size, stability, version, content type, and byte source. Binding rules live in `docs/contracts/30-projection-tree.md`.

Projected files must behave like real files for normal tools. The hard part is not returning bytes. It is returning metadata that does not cause tools to make wrong decisions before they read.

## Model

A projected file has these facts:

- size: exact, known non-empty, or unknown
- stability: stable, dynamic, or live
- version evidence: optional opaque token for one observation
- content type: the requested representation
- byte source: inline, whole-file deferred, ranged deferred, canonical, or blob-backed

These are provider-declared facts, not frontend instructions. The host and tree derive cache placement, direct I/O, learned-size publication, and protocol attributes from them.

## Tool compatibility

The product contract is judged by common tools, not one ideal caller.

Tools that stream from offset 0 can often work with unknown size: `cat`, `head`, `grep`, `jq`, `xxd`, and similar readers.

Tools that decide from stat data require exact size to be fully correct:

| Tool mode | Why non-exact size is not enough |
|---|---|
| `tar c` | Tar writes the archive header size before reading file bytes. |
| `wc -c` | Some implementations can use `fstat` and avoid reading bytes. |
| `tail -n`, `tail -c`, `less` | These modes can seek from the reported end of file. |
| `du`, `find -size`, `rsync --size-only` | These modes are intentionally metadata-driven. |

There is no POSIX stat value for "unknown length" or "known non-empty but unknown length." The host uses the smallest useful sentinel until the exact length is learned.

## Size policy

Exact size means the provider knows the byte length for this file observation. Reads that contradict an exact size are provider contract errors.

Non-zero means the provider knows the file is not empty, but does not know the length.

Unknown means the provider has no length information.

For non-zero and unknown sizes, the host reports a non-zero sentinel before materialization so readers do not skip the file as empty. That value is a compatibility hint, not a read bound. Read termination must come from the provider response or protocol EOF, never from the sentinel.

## Learned sizes

The host can publish a learned exact size only after a complete observation:

- a whole-file deferred read learns exact size from the returned byte length
- a ranged read learns exact size only when the ranged protocol proves EOF
- live files do not publish durable learned sizes

Learned sizes belong in shared tree and file-attr policy. FUSE and NFS may express attrs differently, but they must not independently decide whether a learned size is authoritative.

## Stability

Stable bytes do not change for the file identity. The host may cache them until capacity eviction or explicit invalidation.

Dynamic bytes may change between observations, but one observation should be internally consistent. Without version evidence or invalidation coverage, dynamic content is observation-scoped.

Live bytes may change while being observed. Live files require ranged reads and must not be treated as whole-file cache entries.

Version tokens are opaque evidence for one dynamic observation. They are cache-key material only when the file's stability and read mode make that meaningful.

## Inline bytes

Inline bytes carry the payload inside the projection result. They must have exact size equal to the byte length and must stay below the SDK's inline byte cap.

Inline is for small, already-known payloads. Larger content should use a whole-file or ranged deferred byte source.

## Rationale

The old failure mode was replacing unknown length with a large fake size. That made tools like `tail` and `tar` trust a lie. The current model prefers a small, truthful sentinel plus post-read size learning.

The other old failure mode was compressing "dynamic" and "live" into one volatility bit. Stable large files, dynamic snapshots, and live logs need different read and cache behavior, so stability is a first-class file fact.

## Rejected shapes

- fake large stat sizes for unknown files
- read termination based on stat-size guesses
- frontend-local learned-size policy
- live files served through inline bytes or whole-file reads
- provider-local caches for projected file bytes
