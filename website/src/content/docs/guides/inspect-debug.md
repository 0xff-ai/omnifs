---
title: Inspect and debug
description: Diagnose a running omnifs with status, doctor, logs, and the inspect event stream — plus user-visible probes.
---

When something looks off, omnifs gives you several lenses on a running system:
`status` for mounts and providers, `doctor` for the environment, `logs` for
runtime output, and `inspect` for a live event stream. Start with a quick probe,
then drill in.

```bash
omnifs status
omnifs doctor
```

## `omnifs status`

Summarizes mounts, providers, and the runtime. Add `--detailed` to include cache
statistics.

```bash
omnifs status
omnifs status --detailed
```

Use this first to confirm a mount is present and its provider is loaded. If
nothing appears under a mount, this tells you whether the mount is even
configured.

## `omnifs doctor`

Runs environment and auth diagnostics — checks the runtime, credentials, and the
prerequisites omnifs needs. `--fix` attempts to repair detected issues.

```bash
omnifs doctor
omnifs doctor --fix
```

## `omnifs logs`

Shows runtime container output. Follow with `-f` and limit with `-n`. Clone
failures and provider errors surface here.

```bash
omnifs logs -f
omnifs logs -n 80
```

## `omnifs inspect`

Streams the live FUSE / provider / callout event stream as JSONL. Filter by kind
or mount, limit the count, or follow it.

```bash
omnifs inspect                       # follow all events
omnifs inspect --kind fuse           # only FUSE events
omnifs inspect --kind provider
omnifs inspect --kind callout
omnifs inspect --mount /github       # only events for a mount
omnifs inspect --limit 50            # stop after 50 events
```

| `--kind` value | Shows |
|----------------|-------|
| `fuse` | Filesystem operations (lookups, reads, listings) |
| `provider` | Provider dispatch decisions |
| `callout` | Outbound calls (HTTP fetch, git open) |

Use `inspect` when a path is slow or behaves oddly and you want to see exactly
which lookups, provider calls, and outbound callouts it triggers.

## User-visible probes

Before theorizing, reproduce the problem with a plain read and watch the runtime
log. The runtime writes detailed traces to `/tmp/omnifs.log` inside the
container.

```bash
# Reproduce by navigating to the path
cd /github/<owner>
cat /dns/@google/<domain>/MX

# Then read the runtime log inside the container
omnifs shell tail -n 80 /tmp/omnifs.log
```

:::note
`omnifs logs` (and `docker logs omnifs`) show entrypoint stdout/stderr; the
detailed FUSE traces live in `/tmp/omnifs.log` *inside* the container, reachable
via `omnifs shell`. FUSE `access(...)` warnings in the log are expected noise
unless they line up with a real failure.
:::

:::tip
For SSH, mount, and `Input/output error` problems specifically, see
[Troubleshooting](/guides/troubleshooting/).
:::
