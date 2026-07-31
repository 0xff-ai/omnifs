# File attributes

Status: current-architecture
Scope: why projected file attrs are shaped as size, stability, version, content
type, and byte source.

Read when: changing projected attrs, stat sizes, read termination, stability,
version evidence, inline bytes, or learned size publication.

Binding contracts: `docs/contracts/30-projection-tree.md`.

Projected files return metadata that does not mislead tools before a read.

## Model

| Fact | Values | Consequence |
|---|---|---|
| Size | Exact, non-empty unknown, unknown | Stat value and size learning |
| Stability | Stable, dynamic, live | Cache scope and direct I/O |
| Version | Optional opaque observation token | Dynamic cache identity |
| Content type | Requested representation | Render and response metadata |
| Byte source | Inline, whole-file, ranged, canonical, blob | Read path |

Providers declare these facts. The tree derives cache placement, direct I/O,
learned sizes, and protocol attrs; filesystems only encode the result.

## Tool compatibility

Offset-zero readers such as `cat`, `head`, `grep`, `jq`, and `xxd` can often
work with unknown size. Tools that decide from stat data require exact size to
be fully correct:

| Tool mode | Why non-exact size is not enough |
|---|---|
| `tar c` | Tar writes the archive header size before reading file bytes. |
| `wc -c` | Some implementations can use `fstat` and avoid reading bytes. |
| `tail -n`, `tail -c`, `less` | These modes can seek from the reported end of file. |
| `du`, `find -size`, `rsync --size-only` | These modes are intentionally metadata-driven. |

## Size and learning

POSIX has no unknown-length stat value. Exact means the provider knows the byte
length for this observation; a contradictory read is a provider contract
error. Non-zero means only that content exists; unknown carries no length fact.
The latter two report `1` until materialization so stat-driven readers do not
skip the file. This is a compatibility hint, never a read bound.

The host can publish a learned exact size only after a complete observation:

- a whole-file deferred read learns exact size from the returned byte length
- a ranged read learns exact size only when the ranged protocol proves EOF
- live files do not publish durable learned sizes

Read termination comes from the provider response or protocol EOF. Learned-size
authority stays in shared tree policy; FUSE and NFS only encode it.

## Stability

| Stability | Meaning | Cache rule |
|---|---|---|
| Stable | Bytes do not change for the file identity | Cache until eviction or invalidation |
| Dynamic | Bytes may change between observations | Without version or invalidation evidence, scope to one observation |
| Live | Bytes may change during observation | Require ranged reads; never cache as a whole file |

Version tokens are opaque evidence for one dynamic observation and become
cache-key material only when stability and read mode support reuse.

## Inline bytes

Inline bytes carry a small, already-known payload in the projection result.
Their exact size is the payload length, and the SDK cap bounds the payload.
Larger content uses a whole-file or ranged deferred source.

The model separates dynamic snapshots from live streams because they need
different read and cache behavior. It uses `1`, rather than a large guessed
size, because tools such as `tail` and `tar` treat stat size as authoritative.

## Rejected shapes

- fake large stat sizes for unknown files
- read termination based on stat-size guesses
- filesystem-local learned-size policy
- live files served through inline bytes or whole-file reads
- provider-local caches for projected file bytes
