---
title: "CLI reference"
description: "The main omnifs commands for setup, lifecycle, credentials, logs, inspection, status, and provider configuration."
---

```text
omnifs <command> [options]
```

Use `-v` or `-vv` before a command for more verbose tracing.

## Main commands

| Command | Purpose |
|---|---|
| `omnifs setup` | Guided onboarding: pick providers, configure mounts, optionally start runtime. |
| `omnifs init` | Configure one provider mount. |
| `omnifs up` | Start the runtime container and materialize credentials. |
| `omnifs dev` | Contributor-only local dev sandbox. Requires a source checkout. |
| `omnifs shell` | Open a shell inside the running runtime container. |
| `omnifs down` | Stop and remove the runtime container and session dir. |
| `omnifs status` | Print mount, provider, auth, and runtime status. |
| `omnifs logs` | Print or follow runtime logs. |
| `omnifs inspect` | Attach to or replay the inspector event stream. |
| `omnifs auth` | Manage provider credentials. |
| `omnifs mounts` | Manage configured mounts. |
| `omnifs reset` | Remove mount config and credentials. |
| `omnifs doctor` | Diagnose environment and auth. |
| `omnifs completions` | Print shell completions. |
| `omnifs version` | Print version information. |

## Verbosity

Verbose flags are passed before the command:

```bash
omnifs -v status
omnifs -vv logs -f
```

Use verbosity when debugging runtime startup, provider loading, or credential materialization.

## setup

```text
omnifs setup [--no-up] [-y|--yes] [--providers <name,...>] [--no-browser]
```

`setup` is re-runnable. Already configured providers are shown but skipped by the picker.

## init

```text
omnifs init [provider] [--as <name>] [--no-input] [--yes] [--no-browser]
              [--token <src> | --token-env <ENV_VAR>]
              [--scope <scope>]...
              [--providers-dir <path>] [--mounts-dir <path>]
```

Use `--token -` to read a static token from stdin. Use `--scope` for OAuth scopes when the provider supports them.

## up

```text
omnifs up [--mounts-dir <path>] [--providers-dir <path>]
          [--image <ref>] [--container-name <name>]
```

`up` rewrites mount config for the session, materializes host-managed credentials into a session directory, and starts the runtime container.

Use `--image` to override the runtime image during release or runtime-image testing. Normal users should not need it.

## shell

```text
omnifs shell [--container-name <name>] [command...]
```

Without a command, opens an interactive shell. With a command, runs it inside the container.

## status

```text
omnifs status [--container-name <name>] [--mount-point <path>]
              [--config-dir <path>] [--cache-dir <path>]
              [--detail] [--json]
```

Use `--json` for machine-readable output.

## logs

```text
omnifs logs [--container-name <name>] [-f|--follow]
```

`--follow` tails `/tmp/omnifs.log` inside the runtime container.

## inspect

```text
omnifs inspect [--container-name <name>] [--plain]
               [--record <file>] [--replay <file>]
```

Use `--plain` for JSONL output, `--record` to save a live stream, and `--replay` to inspect a captured stream.

Inspector output can include paths, provider names, upstream remotes, cache outcomes, and provider error strings. Treat recorded streams as operational traces, not sanitized public logs.

## auth

```text
omnifs auth status [--json]
omnifs auth login <mount> [--no-browser] [--scope <scope>]...
omnifs auth logout <mount> [--revoke]
omnifs auth refresh <mount>
omnifs auth scopes <mount>
omnifs auth import <mount> [--token <src> | --token-env <ENV_VAR>] [--scheme <scheme>]
```

Use `--token -` to read a static token from stdin.

## mounts

```text
omnifs mounts rm <name> [--force] [--keep-credentials]
```

## reset

```text
omnifs reset [-y|--yes] [--keep-credentials] [--container-name <name>]
```

## doctor, completions, and version

```text
omnifs doctor [--json]
omnifs completions <shell>
omnifs version
```

Use `doctor` for environment checks before debugging provider code. Use completions from a trusted local install, not from a copied docs snippet.

## Hidden commands

`daemon` and `debug` exist for internal/runtime use and are intentionally omitted from public command examples.
