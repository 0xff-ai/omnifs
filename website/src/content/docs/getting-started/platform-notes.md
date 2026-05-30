---
title: Platform notes
description: Where the omnifs mount lives on Linux and macOS, and why the CLI runs on the host while the filesystem runs in a container.
---

omnifs always splits responsibilities the same way: the **CLI runs on your
host**, and the **filesystem mount runs inside a Linux container**. What differs
between platforms is where that container's Linux kernel comes from and how you
reach the mount.

You browse the mount from inside the container — typically via `omnifs shell` —
not from a path on your host filesystem.

## Linux

On Linux the Docker daemon uses the host Linux kernel directly, so FUSE runs
natively inside the container. The mount lives inside the container; reach it
with `omnifs shell`.

## macOS

On macOS, Docker Desktop runs a Linux VM, and the omnifs container runs inside
that VM. FUSE runs inside the VM's container, so `/github`, `/dns`, and the
other mounts live inside the container — **not** as a native Finder mount or a
path in your host shell.

:::caution
On macOS, do not expect `/github` to appear in Finder or in your host terminal.
Use `omnifs shell` to enter the container and browse the mount there.
:::

Reach the mount the same way on both platforms:

```bash
omnifs shell
> cd /github/torvalds
> ls
```

## Windows

Windows support is planned but not yet available.

## Why the split

Keeping the mount in a container means omnifs uses Linux FUSE everywhere,
including on macOS, instead of depending on macFUSE or platform-specific mount
behavior. The host CLI stays small and portable: it manages credentials,
generates mount configs, pulls the runtime image, and starts and stops the
container, while the container owns the actual filesystem.

## Related

- [Prerequisites](/getting-started/prerequisites/) for Docker and SSH agent
  setup.
- [Install](/getting-started/install/) for how the CLI and runtime image are
  distributed.
- [Quickstart](/getting-started/quickstart/) for the end-to-end flow.
