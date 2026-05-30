---
title: Prerequisites
description: What you need before installing omnifs — Node.js and npm, Docker, and an SSH agent with a GitHub key for repo clones.
---

omnifs ships as a native CLI you install with npm. The CLI runs on your host
(macOS or Linux) and manages a Linux runtime container where the filesystem is
actually mounted. Before you install, make sure the host has the pieces below.

## Node.js and npm

The packaged CLI is published to npm as `@0xff-ai/omnifs`, so you need Node.js
and npm to install it globally.

```bash
node --version
npm --version
```

Any current LTS release of Node.js works. npm installs only the native host
binary for your platform; it does not pull the Docker image (see
[Install](/getting-started/install/)).

## Docker

The omnifs mount runs inside a Linux container, so Docker is required on every
platform. The CLI pulls and starts the runtime image for you on `omnifs up`.

- **Linux**: install Docker Engine. The container uses the host Linux kernel for
  FUSE directly.
- **macOS**: install Docker Desktop. The container runs inside Docker Desktop's
  Linux VM, and you reach the mount through `omnifs shell` rather than as a
  native Finder mount.

Confirm Docker is installed and the daemon is running:

```bash
docker --version
docker info
```

If `docker info` errors, start Docker (or Docker Desktop) before continuing.

:::note
Windows is planned but not yet supported.
:::

## SSH agent with a GitHub key

The GitHub provider clones repositories over SSH from inside the container,
using your forwarded SSH agent socket. This is only required if you want to
browse repo trees under `/github/<owner>/<repo>/repo/`. Other paths (issues,
pulls, CI runs) and other providers do not need it.

omnifs forwards your agent socket into the container rather than copying your
private key. The container can ask your agent to sign while the socket is
mounted, but your key never leaves the host.

Verify the agent is running and a usable GitHub key is loaded:

```bash
echo "$SSH_AUTH_SOCK"
ssh-add -L
ssh -T git@github.com
```

- `echo "$SSH_AUTH_SOCK"` should print a socket path, not an empty line.
- `ssh-add -L` should list at least one key.
- `ssh -T git@github.com` should greet you by username (GitHub does not grant
  shell access, so a "successfully authenticated" message is the expected
  result).

:::tip
If `SSH_AUTH_SOCK` is empty, start an agent and add your key:

```bash
eval "$(ssh-agent -s)"
ssh-add ~/.ssh/id_ed25519
```
:::

## Next steps

With Node.js, Docker, and (optionally) your SSH agent ready, continue to
[Install](/getting-started/install/).
