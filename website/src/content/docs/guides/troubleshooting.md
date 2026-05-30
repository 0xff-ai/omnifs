---
title: Troubleshooting
description: Fix common omnifs problems — SSH agent forwarding, Input/output errors on repo paths, missing mounts, and macOS visibility.
---

Most omnifs problems fall into a few buckets: SSH not reaching GitHub, a repo
path erroring because the clone failed, a mount not being configured, or
confusion about where the mount lives on macOS. Work through the matching section
below.

## `omnifs up` fails

Check that Docker is running:

```bash
docker ps
```

## SSH agent not forwarded / `ssh -T git@github.com` fails

omnifs clones GitHub repos over SSH using your forwarded SSH agent. If SSH does
not work on your host, it will not work in the container.

```bash
# Verify your SSH agent has a GitHub key loaded
ssh-add -L

# Verify you can reach GitHub over SSH from the host
ssh -T git@github.com
```

Both must succeed on the host. omnifs forwards the agent socket
(`SSH_AUTH_SOCK`) into the container and does **not** mount private keys, so the
agent is the only path for clone auth.

:::caution
If `ssh -T git@github.com` fails on your host, fix that first (load a key with
`ssh-add`, confirm the key is registered with GitHub). No omnifs setting can work
around a broken host SSH agent.
:::

## `Input/output error` on a repo path

This almost always means the repository could not be cloned. Diagnose in order:

```bash
# 1. Check the runtime output for the git clone stderr
omnifs logs            # or: docker logs omnifs

# 2. Check SSH from inside the container
omnifs shell ssh -F /dev/null -T git@github.com

# 3. Confirm the mount is still present
omnifs shell cat /proc/mounts
```

The clone stderr surfaces in the runtime log; the detailed trace is in
`/tmp/omnifs.log` inside the container (`omnifs shell tail -n 80 /tmp/omnifs.log`).

## Repos are slow to clone

Large repos take time to clone on first access. The clone runs in the
background, and the path becomes available once it completes. Subsequent reads
are cached and fast.

## Nothing appears under a mount

Check that the mount is configured and its provider is loaded:

```bash
omnifs status
```

If the mount is missing, add it with `omnifs init <provider>` or
`omnifs mounts add <provider>` (see [Managing mounts](/guides/managing-mounts/)).
If it is present but disabled, enable it with `omnifs mounts enable <mount>`.

## macOS: paths not visible in Finder

omnifs runs the FUSE mount inside a Linux container, so the projected paths live
inside that container — not directly on your macOS host. Browse them with the
CLI:

```bash
omnifs shell
omnifs shell ls /github/torvalds
```

:::note
For a deeper look at a running system — the live event stream, cache stats, and
environment probes — see [Inspect and debug](/guides/inspect-debug/).
:::
