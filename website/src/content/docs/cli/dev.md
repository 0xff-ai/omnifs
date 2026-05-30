---
title: Dev sandbox
description: omnifs dev — the contributor-only local sandbox that builds the dev image and launches all built-in providers.
---

```bash
omnifs dev [flags]
```

`omnifs dev` brings up the canonical local development sandbox. It is
contributor-only and requires a source checkout — it walks up from the current
directory to find the workspace `Cargo.toml` and operates relative to it. This is
distinct from the user `up` path: it builds an image from source, wires in dev
secrets and fixtures, and mounts every built-in provider, rather than
materializing user-configured mounts.

## What it does

In one flow, `omnifs dev`:

1. walks up to the workspace `Cargo.toml`,
2. builds the dev image, tagged `omnifs:<short-sha>-dev`,
3. captures `gh auth token` into `.secrets/github_token`,
4. downloads the Chinook SQLite fixture into `.secrets/db/test.db`,
5. synthesizes dev mount configs from the built-in provider manifests,
6. launches the container with all built-in providers' dev mounts, plus the
   Docker socket and the DB fixture exposed.

## Flags

| Flag | Purpose |
|------|---------|
| `-y`, `--yes` | Skip the confirmation prompt before building and launching. |

## Examples

```bash
omnifs dev           # build the dev image, materialize secrets/fixtures, launch
omnifs dev -y        # same, without the confirmation prompt
```

Once it is running, the standard lifecycle verbs work against the dev container:

```bash
omnifs shell         # attach a shell and browse the providers
omnifs logs -f       # follow runtime logs
omnifs status        # inspect mounts and providers
omnifs down          # stop and remove the container
```

:::caution
`omnifs dev` is for contributors working in a source checkout. End users should
configure mounts with [`omnifs setup`](/cli/onboarding-config/) or
[`omnifs init`](/cli/onboarding-config/) and launch with
[`omnifs up`](/cli/lifecycle/).
:::

:::note
The captured `gh auth token` is exposed inside the container as a read-only
mounted secret. Host private keys are never mounted; git clones use the forwarded
SSH agent socket.
:::
