# host-cache-bench

Microbenchmark harness for omnifs host-cache backend candidates.

Five backends, six workloads, single binary. See
`design/host-cache-bench.md` for analysis and recommendations and
`design/host-cache.md` for the design proposal this validates.

## Quickstart

```
cargo build --release
./target/release/host-cache-bench --n 30000
```

Pass `--workloads` and `--backends` (repeatable) to scope the run.
Pass `--workdir <PATH>` to keep the per-(backend, workload) DBs around
for poking at.

`results-n30k.txt` has the most recent full-matrix run output.

## Backends

- `redb-naive` — current in-tree shape: one redb DB, three tables,
  kind-prefixed string keys.
- `redb-split` — proposed: metadata + path-to-blob + content-addressed
  blob with refcount; no compression.
- `redb-split-zstd` — as `redb-split`, plus zstd-3 on blob payloads
  ≥ 4 KiB.
- `sqlite` — rusqlite WAL mode with the same logical schema.
- `fjall-split` — fjall LSM with KV separation on the blob keyspace.

## Workloads

- `bulk-preload` — write throughput from terminal-application bursts.
- `hot-read` — Zipfian read pattern + cold-restart pass + reopen time.
- `mixed` — 70/25/5 read/write/delete.
- `prefix-invalidate` — webhook-style prefix deletes.
- `file-dedup` — 50% paths share content; tests dedup.
- `large-blob` — 4 KiB – 512 KiB file payloads; tests big-value path.
