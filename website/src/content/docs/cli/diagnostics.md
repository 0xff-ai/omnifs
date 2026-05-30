---
title: Diagnostics
description: Inspect your omnifs setup — doctor environment probes, version detail, and shell completions.
---

These verbs help you confirm omnifs is set up correctly and wire it into your
shell.

## `omnifs doctor`

```bash
omnifs doctor
```

Probes the environment and auth state and prints each result with a status glyph
(`✓` ok, `⚠` warning, `✗` failure, `·` skipped). It runs in dependency order and
does not attempt any auto-fix — it diagnoses, you remediate. It takes no flags.

Probes, in order:

| Probe | Checks |
|-------|--------|
| docker reachable | The Docker daemon responds to a ping. |
| fuse | `/dev/fuse` exists and is openable (Linux only; skipped on macOS). |
| image cached | The runtime image is present locally; warns (not fails) if it will be pulled on `up`. Skipped if Docker is unreachable. |
| providers discovered | Built-in and on-disk providers are found. |
| keychain backend | The OS keychain is available, else warns and notes the file fallback. |
| ssh-agent | `SSH_AUTH_SOCK` is set and the socket exists (used for git clones). |
| config file | Reports the `config.toml` path, or that the default is in effect. |
| mount configs valid | Each configured mount parses and loads. |
| auth ready (per mount) | Each valid mount has a usable credential. Reuses the same loaders as `status`. |
| network | Best-effort `ghcr.io` reachability; only ever warns, never fails. |

```bash
omnifs doctor
```

Exit codes: `0` if everything is green or skipped, `1` if any probe is red, `2`
if there are no failures but at least one warning.

## `omnifs version`

```bash
omnifs version [--detail]
```

Prints version information. With no flags it prints one line, the CLI version.
`--detail` expands it with the resolved runtime image, container name, credential
store backend, provider count, and resolved directories.

| Flag | Purpose |
|------|---------|
| `--detail` | Show image, container, store backend, provider count, and dirs. |

```bash
omnifs version
# omnifs 0.2.0

omnifs version --detail
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
