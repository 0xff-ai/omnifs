# Warm-latency measurement suite

Reproducible warm p50/p95 and a cold first-touch number for `ls`, `cat`, and
`grep -r` against a live omnifs mount, at concurrency 1/4/8. It answers "does
the projected tree respond like real files, fast enough, under concurrency?"

`run.ts` times **real spawned processes** (`ls`, `cat`, `grep -r`) with
`performance.now()`. There are no shell pipelines: each sample is one
`Bun.spawn` of the actual command, so a sample is the wall time a user pays to
invoke that tool, including the tool's own `fork`/`exec`. Thresholds are
**recorded, not enforced** (see below).

## What it measures

Four scenarios, discovered from `--target` (or pinned with overrides):

| scenario    | command                              |
|-------------|--------------------------------------|
| `ls`        | `ls <target>`                        |
| `ls_subdir` | `ls <target>/<first-subdir>`         |
| `cat`       | `cat <first-file>`                   |
| `grep_r`    | `grep -r <literal> <target-subdir>`  |

Per scenario:

- **Cold** (`cold_first_ms`): the very first spawn of that scenario this run.
  It is a true first touch only when nothing read the path beforehand (see the
  cold protocol). Recorded on the lowest-concurrency row of each scenario; the
  higher-concurrency rows carry `null`.
- **Warm** (`p50_ms` / `p95_ms` / `n`): after one untimed warm-up, `--iterations`
  timed iterations per concurrency level. At concurrency `C` each iteration
  launches `C` copies simultaneously with `Promise.all` and records every
  duration, so `n = iterations * C`. Percentiles are nearest-rank.

## Output

`--out <file.json>` writes the JSON report and a Markdown table beside it
(`<file>.md`). JSON shape:

```json
{
  "date": "YYYY-MM-DD",
  "target": "/omnifs",
  "git_sha": "…",
  "host": "Linux aarch64 (container)",
  "iterations": 50,
  "concurrencies": [1, 4, 8],
  "discovery": { "subdir": "…", "file": "…", "grep_literal": "…", "from_overrides": true },
  "scenarios": [
    { "name": "ls", "concurrency": 1, "warm": { "p50_ms": 1.0, "p95_ms": 1.5, "n": 50 }, "cold_first_ms": 2.9 }
  ]
}
```

## Running it

### Timing fidelity: run where the mount is local

The suite must run on the same host as the mount so that per-op timing is not
polluted by transport overhead. Concretely:

- **Docker-hosted filesystem** (`just dev`): the mount lives at `/omnifs` inside
  the credential-free filesystem container. Driving one `docker exec` per
  operation would add hundreds of milliseconds of startup to every sample and
  swamp millisecond-scale filesystem ops. Run the suite inside that container.
  The filesystem image has no Bun, so compile `run.ts` to a standalone Linux
  binary on the host and copy it in.
- **Host-native mount** (Linux FUSE or macOS NFSv4 loopback): the mount is a
  host path, so run `run.ts` directly with `bun`.

### Docker-hosted filesystem

The concrete k8s fixture paths below are available on Linux. `scripts/dev.ts`
skips the k8s mount on macOS because Docker Desktop cannot expose its Unix
socket to the host-native daemon.

```bash
# 1. Bring the daemon and named filesystems up without opening a shell,
#    then discover the exact dev Docker filesystem container.
just dev -y --detach
FS_CONTAINER=$(docker ps \
  --filter label=ai.0xff.omnifs.home="$HOME/.omnifs-dev" \
  --filter label=ai.0xff.omnifs.fs=dev-docker \
  --format '{{.Names}}')
test -n "$FS_CONTAINER"

# 2. Compile the suite for the container's arch (linux/arm64 on Apple silicon,
#    linux/x64 on Intel) and copy the single self-contained binary in.
bun build --compile --target=bun-linux-arm64 benchmarks/latency/run.ts \
  --outfile /tmp/latency-bench
docker cp /tmp/latency-bench "${FS_CONTAINER}:/tmp/latency-bench"
docker exec "$FS_CONTAINER" chmod +x /tmp/latency-bench

# 3. Run it against the mount, pinning the paths for a clean cold number
#    (see Cold protocol). Pass the host git sha since there is no repo inside.
docker exec "$FS_CONTAINER" /tmp/latency-bench \
  --target /omnifs \
  --subdir /omnifs/k8s/cluster/apiservices \
  --file /omnifs/k8s/cluster/apiservices/v1.apps/manifest.json \
  --grep-literal apiVersion \
  --iterations 50 --concurrency 1,4,8 \
  --git-sha "$(git rev-parse HEAD)" \
  --out /tmp/latency-$(date +%F).json

# 4. Copy the report out and commit it under benchmarks/reports/.
docker cp "${FS_CONTAINER}:/tmp/latency-$(date +%F).json" benchmarks/reports/
docker cp "${FS_CONTAINER}:/tmp/latency-$(date +%F).md" benchmarks/reports/
```

Pin `--subdir` at a **local fixture** provider (the dev `k8s` mount is a local
k3s cluster) to measure omnifs's own overhead rather than upstream API latency.
The network-backed mounts (`arxiv`, `dns`, `github`) are valid targets too;
there, cold reflects the upstream fetch and warm reflects the host cache.

### Host-native mount

```bash
# Copy the host location from `target/debug/omnifs fs ls`.
MOUNT=/absolute/path
bun benchmarks/latency/run.ts \
  --target "$MOUNT" \
  --iterations 50 --concurrency 1,4,8 \
  --out benchmarks/reports/latency-$(date +%F).json
```

## Cold protocol

`cold_first_ms` is only a true first touch if nothing read the path before the
timed spawn. Two things guarantee that:

1. **Restart the daemon with the Filesystem still desired.** Run
   `target/debug/omnifs down`, then `target/debug/omnifs status` with the same
   `OMNIFS_HOME`; status starts the daemon and waits for readiness without
   reading the mount. The durable cache under `<profile>/daemon-state/cache`
   persists, so this gives *fresh-process cold* (in-memory/session state reset,
   first provider callout), not *upstream-cold*. To drop durable cache too,
   remove that directory while the daemon is down.
2. **Pin `--subdir`, `--file`, and `--grep-literal`.** With all three set, the
   suite reads no tree bytes before timing (it only `stat`s the three paths to
   validate them, which is the `getattr` any `ls`/`cat` does anyway). Without
   them, the suite auto-discovers by reading the tree, which warms listings and
   the sampled file first; the report then flags `from_overrides: false` and the
   cold numbers as approximate.

## Options

| flag              | default   | meaning                                                        |
|-------------------|-----------|----------------------------------------------------------------|
| `--target`        | required  | mounted omnifs directory                                       |
| `--out`           | required  | JSON report path; a `.md` table is written beside it           |
| `--concurrency`   | `1,4,8`   | comma list drawn from `{1,4,8}`                                 |
| `--iterations`    | `50`      | timed iterations per (scenario, concurrency)                   |
| `--warmup`        | `1`       | untimed warm-up runs per scenario                              |
| `--subdir`        | discovered| first-subdir override (absolute or relative to `--target`)     |
| `--file`          | discovered| file-to-cat override                                           |
| `--grep-literal`  | sampled   | grep literal override                                          |
| `--git-sha`       | `git`/env | sha to record (else `OMNIFS_GIT_SHA`, else `git rev-parse`)    |

## Thresholds (recorded, not enforced)

The warm-latency target is **p95 <= 50 ms at concurrency 8**. The suite records
the number and the Markdown table annotates each concurrency-8 row `within` or
`over`; it never fails the run on a threshold. Evaluating the numbers remains a
human decision.
