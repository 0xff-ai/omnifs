---
title: Setup and troubleshooting
description: Diagnose install, Docker, projection, auth, and first-read failures during omnifs setup.
---

Most setup failures fall into one of five buckets: the CLI is not installed, Docker is not reachable, no mount is configured, credentials are missing, or the provider was denied a capability it asked the host to use.

Start with the commands that report current state before changing configuration:

```bash
omnifs doctor
omnifs status --detail
omnifs logs
```

Use `-v` or `-vv` before a command when you need more CLI tracing:

```bash
omnifs -vv up
```

## CLI install

omnifs is distributed through npm:

```bash
npm install -g @0xff-ai/omnifs
omnifs --version
```

If `omnifs` is not found after install, check the global npm bin directory and make sure it is on `PATH`.

## Docker runtime

`omnifs up` starts a Linux runtime container. On macOS, this container is where the FUSE surface lives; use `omnifs shell` to enter it. Native macOS and Windows surfaces are outside the supported runtime path.

Check Docker first:

```bash
docker version
docker ps
```

Then start omnifs again:

```bash
omnifs up
omnifs shell
```

If startup fails, `omnifs logs` shows the runtime log. `omnifs status --detail` shows configured mounts, provider readiness, auth state, and runtime state.

## No providers mounted

If `/omnifs` is empty or the expected provider directory is missing, list configured mounts:

```bash
omnifs mounts list
omnifs status --detail
```

Add a mount with guided setup:

```bash
omnifs setup
```

Or configure one provider directly:

```bash
omnifs init dns
omnifs init github
omnifs up
```

`omnifs setup` requires an interactive terminal. For non-interactive environments, run `omnifs init <provider>` per provider and then `omnifs up`.

## Auth failures

GitHub and Linear use host-managed credentials. The provider does not receive the credential store; the host injects approved headers into approved HTTP callouts.

Check auth state:

```bash
omnifs auth status
omnifs auth scopes github
```

Refresh or log in again:

```bash
omnifs auth refresh github
omnifs auth login github
```

For a static token:

```bash
omnifs auth import github --token-env GITHUB_TOKEN
```

Use `--no-browser` when the login flow should print the OAuth URL instead of opening a browser.

## Capability denials

A provider can only use capabilities granted by the host. A denied HTTP callout, Git handoff, Unix socket, or preopened path usually means the provider manifest, mount config, or runtime grant does not match the operation.

Use the inspector to see the failing event:

```bash
omnifs inspect --plain
```

| Symptom | Likely cause | Check |
|---|---|---|
| HTTP callout denied | Domain is not in the provider capability set | Provider manifest and `omnifs status --detail` |
| Docker paths fail | Docker socket is not configured or reachable | Docker provider config and runtime logs |
| db paths fail | SQLite file is not preopened at the configured guest path | db mount config |
| GitHub `/repo` fails | Git handoff or SSH access failed | SSH agent, GitHub access, runtime logs |

## Cache confusion

A later read may come from the view cache or render from cached canonical object bytes. That is expected. Use `inspect` when you need to distinguish a warm local read from an upstream callout:

```bash
omnifs inspect --plain
cat /omnifs/github/0xff-ai/omnifs/issues/open/42/title
```

Traces redact common secrets, but they can still include paths, object names, remotes, errors, and provider output. Do not share raw traces as if they were sanitized audit artifacts.
