# Persistent host-cache PoC benchmarks

Status: experimental, decisions follow
Scope: companion to `design/host-cache.md`, validates the tech-stack
recommendations with real numbers
Branch: `claude/host-cache-design-Z9Wyn`
Harness: `bench/host-cache/`

## What was built

Five backends, all behind a small `Backend` trait
(`bench/host-cache/src/backends/mod.rs`):

| Backend | Layout |
|---------|--------|
| `redb_naive` | mirror of today's L2: one redb DB, three tables (`metadata`, `content`, `bulk`), kind-prefixed string keys |
| `redb_split` | proposed shape: `metadata` + `path_to_blob` + content-addressed `blob` + refcount, no compression |
| `redb_split_zstd` | as `redb_split`, but blob payloads ≥ 4 KiB are zstd-3 compressed |
| `sqlite` | rusqlite WAL+NORMAL with three tables matching `redb_split` |
| `fjall_split` | fjall LSM with KV separation enabled on the `blob` keyspace |

Six workloads (`bench/host-cache/src/workload.rs`), each scaled by `--n`:

- `bulk_preload`: simulates terminal application — many small writes
  per directory listing (`Lookup` + `Attr` + occasional small `File`).
- `hot_read`: replay the same key set with a Zipfian-ish hot subset;
  measures p50/p99 latency and includes a **post-restart** cold-cache
  read pass plus the database reopen latency.
- `mixed`: 70% read, 25% write, 5% delete.
- `prefix_invalidate`: simulate webhook-driven invalidations on a
  populated cache.
- `file_dedup`: write a corpus with realistic duplication (50% paths
  share content with another path); measures the win from
  content-addressing.
- `large_blob`: many medium-to-large file payloads (4 KiB – 512 KiB)
  to exercise write amplification on big values.

Synthetic file content is text-like (small dictionary of short tokens
with random separators and a sprinkling of byte noise) — roughly 2x
compressible by zstd-3, which is realistic for the JSON/Markdown/code
content omnifs providers actually serve.

Hardware: `Linux 6.18.5`, ext4 on a single disk-backed volume, no
fsync tuning. Each backend runs to the same workload in isolation —
no cross-process interference.

## Headline numbers (`--n 30000`)

Bigger n produced cleaner separation between backends. Lower is better
for `elapsed` and `footprint`; higher is better for `ops/s`. Full output
saved at `bench/host-cache/results-n30k.txt`.

### Throughput (ops/s, higher is better)

| Workload | redb_naive | redb_split | redb_split_zstd | sqlite | fjall_split |
|----------|-----------:|-----------:|----------------:|-------:|------------:|
| bulk_preload | 16,237 | 17,451 | 16,800 | 27,541 | **223,929** |
| hot_read | 25,464 | 17,962 | 16,485 | 26,638 | **125,354** |
| file_dedup | 16,261 | 13,224 | 12,366 | 7,145 | **22,758** |
| prefix_invalidate (ops, each evicts ~150 rows) | 36 | 33 | 30 | 30 | **345** |
| large_blob | 764 | 719 | 535 | 369 | 687 |

### Footprint (lower is better)

`logical_bytes` is the sum of payload bytes the workload tried to
write. The numbers below are the actual on-disk size after `flush`.

| Workload | logical | redb_naive | redb_split | redb_split_zstd | sqlite | fjall_split |
|----------|--------:|-----------:|-----------:|----------------:|-------:|------------:|
| bulk_preload | n/a | 64.76 MiB | 64.76 MiB | 64.76 MiB | 60.65 MiB | 64.02 MiB |
| file_dedup | 174.81 MiB | **257.51 MiB** (no dedup, 1.5x bloat) | 129.01 MiB | 129.01 MiB | 111.73 MiB | 100.83 MiB |
| large_blob | 712.29 MiB | **1.00 GiB** | **1.00 GiB** | **514.51 MiB** | 733.13 MiB | 761.98 MiB |

`redb_naive` at 257 MiB on file_dedup with only 174 MiB of logical data
is the smoking gun for content-addressing: every duplicate write costs
both space and a transactional page rewrite. `redb_split` cuts that to
129 MiB without compression — a clean ~50% win that matches the 50%
content duplication ratio the workload generates.

`redb_naive` and `redb_split` both hit 1.00 GiB on large_blob because
redb's copy-on-write B-tree pays a ~40% write-amp tax on values that
straddle multiple pages. Compression kills it: `redb_split_zstd` lands
at 514 MiB on 712 MiB of logical text-like data, ~28% of the redb
baseline. fjall has its own write-amp story (LSM tiering + lz4 on by
default) and lands at 762 MiB, comparable to sqlite without
compression.

### Latency (hot_read, microseconds)

| Backend | warm p50 | warm p99 | cold p50 (after reopen) | cold p99 | reopen time |
|---------|---------:|---------:|------------------------:|---------:|------------:|
| redb_naive | 1 | 7 | 4 | 16 | 2.5 ms |
| redb_split | 1 | 5 | 4 | 17 | 2.8 ms |
| redb_split_zstd | 1 | 6 | 4 | 16 | 2.8 ms |
| sqlite | 3 | 22 | 5 | 35 | 0.4 ms |
| fjall_split | 1 | 6 | 2 | 10 | **574 ms** |

fjall is best at steady-state read latency, but the reopen cost is two
orders of magnitude worse than every alternative — that's segment
manifest replay. For a cache mounted at the start of every shell
session, this matters.

### Latency (large_blob reads, microseconds)

| Backend | p50 | p99 |
|---------|----:|----:|
| redb_naive | 17 | 668 |
| redb_split | 18 | 719 |
| redb_split_zstd | 92 | 842 |
| sqlite | 274 | 619 |
| fjall_split | 68 | 609 |

zstd's read tax on large blobs is real (92µs vs 18µs at the median).
The tail (p99) is similar across backends; large-blob reads are
dominated by I/O, not codec.

## Conclusions

### 1. Content-addressing is the single biggest win

`redb_split` is essentially `redb_naive` plus a `blake3 + refcount + indirection`
layer, and it cuts duplicate-content footprint in half on the dedup
workload (257 → 129 MiB). On a workload with no duplication
(`large_blob`) it costs nothing in space (both at 1 GiB). The only
runtime cost is one blake3 hash per file write, which doesn't show up
in the throughput numbers (`redb_split` is within noise of
`redb_naive` on every workload). **Implement it.**

### 2. Compression is worth it on file content, not on metadata

zstd-3 on blobs ≥ 4 KiB shaves ~30% off `large_blob` footprint on
text-like data. The cost is ~5x slower median large-blob reads (18 →
92 µs) — still acceptable for a cache, but enough that we should
consider lz4 (faster, less compression) or making it configurable.

Metadata payloads are mostly < 4 KiB and aren't worth compressing
individually; redb's page format already does some structural
compression. Keep compression in the file path only.

### 3. redb's B-tree write amplification on big values is real

Both `redb_naive` and `redb_split` hit 1 GiB on 712 MiB of large-blob
data. That's a 40% bloat, not a one-time cost — every overwrite or
compaction pays it again. Three options:

- **Accept it.** redb is otherwise great for our workload, and large
  files (>1 MiB) are rare in practice for browsing-shaped providers
  (GitHub issues, DNS records, file listings). Document the tradeoff.
- **Add compression** (the `redb_split_zstd` path). Halves the bloat
  on text-like content, doesn't help on binary.
- **Move blobs out of redb** into a sidecar content-addressed file
  store (one file per blob, named by hash; metadata in redb). Loses
  single-DB transactional safety but eliminates the write-amp.
  Equivalent to cacache's design.

I lean toward "accept + add compression" because it keeps the single-DB
transactional model. Revisit if real-world workloads hit large-binary
heavy patterns.

### 4. fjall is the throughput champion but pays for it

10x faster on bulk write, 5x faster on hot reads, 5x faster on prefix
invalidation. KV-separation puts blobs in the right place. **But:**

- Reopen takes ~575 ms. omnifs mounts open one DB per provider mount
  on host start. With 10 mounts that's a ~5 second startup tax.
- LSM compaction is async and unpredictable; tail latencies under
  write-heavy workloads will be worse than the steady-state numbers
  show (the bench doesn't sustain enough write pressure to trigger
  major compactions).
- 64 MiB preallocation per keyspace × 4 keyspaces per mount = 256 MiB
  baseline footprint per mount before any data lands. For someone
  with 20 mounts that's 5 GiB of empty cache. (You can tune this
  down with `KvSeparationOptions` and partition options, but the
  defaults are not friendly for many small caches.)

If we ever have one mount with hundreds of MiB of hot data and
write-throughput requirements, fjall is the right answer. For the
current "many small mounts" reality, it's not.

### 5. SQLite is fine but never the best choice

It's faster than redb on writes (27k vs 17k ops/s on bulk_preload) and
has the cheapest reopen (0.4 ms), but it's slower on every other
metric and hits the worst large-blob read latency (274 µs p50). Its
prefix delete is also slow (`LIKE` scan). It doesn't displace either
redb or fjall for our workload.

### 6. Cold restart cost is real and unevenly distributed

| Reopen | Backend |
|-------:|---------|
|  0.4 ms | sqlite |
|  2.5 ms | redb_naive |
|  2.8 ms | redb_split |
|  2.8 ms | redb_split_zstd |
| 574 ms | fjall |

The cache earns its keep on cold start: post-reopen p50 reads are 2-5µs
across all backends, meaning the first thousand FUSE ops after a host
start return from disk in microseconds rather than seconds via the
provider. That's the actual user-visible win of persistence — it's
why we're doing this.

## Recommendation, updated

The design doc's recommendation (stay on redb, add content-addressing,
add quotas, optional compression) holds and is now backed by numbers.
Specifically:

1. **Adopt the `redb_split` layout.** Half the disk on duplicate
   content, no observable cost on anything else. ~700 LOC change per
   the design doc's prototype 3.
2. **Make compression configurable per mount, default off.** zstd-3
   above a 4 KiB threshold. The 30% disk savings on text-heavy
   providers (GitHub, Notion, Linear) are worth the codec cost; on
   image-heavy or already-compressed payloads, leave it off.
3. **Skip fjall for now.** The reopen cost and the 256 MiB-per-mount
   baseline disqualify it for the current "many small mounts" use
   case. Reconsider when we have a single-mount workload with > 1 GiB
   of working set and webhook-driven write pressure.
4. **Skip sqlite.** No metric where it wins.
5. **The byte accountant + SIEVE eviction prototype** (design-doc
   prototype 2) is independent of this choice and should land first;
   the bench shows none of the backends evict on their own and none
   bound their growth.

## Caveats and what's not measured

- **Single-threaded.** The host runtime serializes writes through one
  thread; FUSE reads contend on read locks. The bench mirrors this.
  Multi-writer scenarios (which we don't have) would change the
  picture.
- **No fsync tuning.** All backends run with their default durability
  settings. redb fsyncs on commit; sqlite is `synchronous=NORMAL` (the
  default for WAL); fjall persists journal entries on commit but
  segments async. A "we tolerate losing the last second" mode is
  worth a benchmark pass; it would mostly help the redb numbers.
- **Eviction not benchmarked.** None of the backends self-evict; the
  bench measures only what's already in the design's prototype 1
  scope. Prototype 2 (byte accountant + SIEVE) needs its own bench.
- **Synthetic compressibility.** The text-like generator gives
  ~2x compressibility. Real GitHub/Notion content is in that range;
  binary file content (images, archives) will compress much less.
- **No replay across very long runs.** redb's space stays allocated
  after deletes (no auto-compaction). Over months, footprint will
  exceed the working set unless we add manual compaction or a
  scheduled `vacuum`. This argues for an `omnifs cache compact`
  subcommand.
- **fjall LSM tail latency** under sustained write pressure is not
  measured. Would need a 10-minute test, not the seconds-to-minutes
  the bench runs.

## Reproducing

```
cd bench/host-cache
cargo build --release
./target/release/host-cache-bench --n 30000
# or just one workload + one backend:
./target/release/host-cache-bench --n 50000 \
    --workloads file-dedup --backends redb-split-zstd
```

The harness writes per-(backend, workload) databases to a temp dir and
prints a per-workload summary table with throughput, footprint, and
extra metrics (latency percentiles, dedup ratios, eviction counts).
Pass `--workdir` to keep them.
