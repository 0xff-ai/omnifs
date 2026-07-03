# Latency measurement suite (K3)

Reproducible warm p50/p95 and a cold first-touch number for `ls`, `cat`, and
`grep -r` against a live omnifs mount, at concurrency 1/4/8. This is the K3
instrument from the truth track: it answers "does the projected tree respond
like real files, fast enough, under concurrency?"

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

- **Docker dev runtime** (`just dev`): the mount lives at `/omnifs` *inside* the
  `omnifs` container. Driving one `docker exec` per operation would add hundreds
  of milliseconds of exec startup to every sample and swamp millisecond-scale
  filesystem ops. So the suite runs **inside the container**. The runtime image
  has no Bun, so compile `run.ts` to a standalone Linux binary on the host and
  copy it in.
- **Host-native mount** (macOS NFSv4 loopback): the mount is a real host path,
  so run `run.ts` directly with `bun`.

### Docker dev runtime

```bash
# 1. Bring the runtime up and leave it running (no interactive shell).
just dev -y --detach          # or --no-shell

# 2. Compile the suite for the container's arch (linux/arm64 on Apple silicon,
#    linux/x64 on Intel) and copy the single self-contained binary in.
bun build --compile --target=bun-linux-arm64 benchmarks/latency/run.ts \
  --outfile /tmp/latency-bench
docker cp /tmp/latency-bench omnifs:/tmp/latency-bench
docker exec omnifs chmod +x /tmp/latency-bench

# 3. Run it against the mount, pinning the paths for a clean cold number
#    (see Cold protocol). Pass the host git sha since there is no repo inside.
docker exec omnifs /tmp/latency-bench \
  --target /omnifs \
  --subdir /omnifs/k8s/cluster/apiservices \
  --file /omnifs/k8s/cluster/apiservices/v1.apps/manifest.json \
  --grep-literal apiVersion \
  --iterations 50 --concurrency 1,4,8 \
  --git-sha "$(git rev-parse HEAD)" \
  --out /tmp/latency-$(date +%F).json

# 4. Copy the report out and commit it under benchmarks/reports/.
docker cp omnifs:/tmp/latency-$(date +%F).json benchmarks/reports/
docker cp omnifs:/tmp/latency-$(date +%F).md   benchmarks/reports/
```

Pin `--subdir` at a **local fixture** provider (the dev `k8s` mount is a local
k3s cluster) to measure omnifs's own overhead rather than upstream API latency.
The network-backed mounts (`arxiv`, `dns`, `github`) are valid targets too;
there, cold reflects the upstream fetch and warm reflects the host cache.

### Host-native mount

```bash
bun benchmarks/latency/run.ts \
  --target "$HOME/omnifs" \
  --iterations 50 --concurrency 1,4,8 \
  --out benchmarks/reports/latency-$(date +%F).json
```

## Cold protocol

`cold_first_ms` is only a true first touch if nothing read the path before the
timed spawn. Two things guarantee that:

1. **Restart the runtime so caches are fresh.** For the Docker runtime,
   `docker restart omnifs` restarts the daemon; wait for readiness with
   `omnifs status` (grep for `fuse serving`) rather than `ls`-ing the mount, so
   the readiness check does not warm the tree. Caveat: the on-disk cache under
   `OMNIFS_HOME` persists across a restart, so cold here means *fresh-daemon
   cold* (in-memory/session state reset, first provider callout) rather than
   *upstream-cold*. To also drop the on-disk cache, clear `$OMNIFS_HOME/cache`
   before starting.
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

K3's target is **warm p95 <= 50 ms at concurrency 8**. The suite records the
number and the Markdown table annotates each concurrency-8 row `within` or
`over`; it never fails the run on a threshold. Evaluating the numbers is a human
decision (see the truth-track plan, step T4).
