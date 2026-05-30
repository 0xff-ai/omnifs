---
title: Diagnostics
description: Inspect your omnifs setup — doctor environment probes, version detail, and shell completions.
---

These verbs help you confirm omnifs is set up correctly and wire it into your
shell.

## `omnifs doctor`

```bash
omnifs doctor [flags]
```

Probes the environment and auth state and prints a table of results. It runs a
small set of probes in dependency order and does not attempt any auto-fix — it
diagnoses, you remediate.

Probes:

| Probe | Checks |
|-------|--------|
| `docker_reachable` | The Docker daemon is reachable. |
| `image_cached` | The runtime image is present locally. Depends on `docker_reachable`. |
| `mount_configs_valid` | Configured mounts parse and load. |
| `auth_ready` | Each mount has a usable credential. Reuses the `status` loaders; depends on `mount_configs_valid`. |
| `ssh_agent` | An SSH agent is available (used for git clones). |
| `network` | Best-effort connectivity check; never reported as a failure. |

| Flag | Purpose |
|------|---------|
| `--json` | Emit a machine-readable payload. |
| `--container-name <NAME>` | Container name. Defaults to `OMNIFS_CONTAINER_NAME`, then `omnifs`. |

```bash
omnifs doctor
omnifs doctor --json
```

Exit codes: `0` if everything is green or skipped, `1` if any probe is red, `2`
if there are no failures but at least one warning.

## `omnifs version`

```bash
omnifs version [--detail] [--json]
```

Prints version information. With no flags it prints one line, the CLI version.
`--detail` expands it with the resolved runtime image, container name, credential
store backend, and provider count.

| Flag | Purpose |
|------|---------|
| `--detail` | Show image, container, store backend, and provider count. |
| `--json` | Emit a machine-readable payload. |

```bash
omnifs version
# omnifs 0.2.0

omnifs version --detail
omnifs version --json
```

:::note
The plain output is a single line, mirroring `gh` and `docker`. The CLI version,
npm package version, and default image tag all share the same semver.
:::

## `omnifs completions`

```bash
omnifs completions <shell>
```

Generates a shell completion script on stdout. Redirect it into the location your
shell loads completions from.

| Argument | Purpose |
|----------|---------|
| `<shell>` | Target shell: `bash`, `zsh`, `fish`, and others supported by `clap_complete`. |

```bash
# zsh
omnifs completions zsh > "${fpath[1]}/_omnifs"

# bash
omnifs completions bash > /etc/bash_completion.d/omnifs

# fish
omnifs completions fish > ~/.config/fish/completions/omnifs.fish
```
