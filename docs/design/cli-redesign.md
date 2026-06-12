# CLI design

Status: accepted
Scope: `crates/omnifs-cli`
Related: `docs/design/mount-lifecycle.md`, `docs/design/host-auth.md`, `docs/design/npm-distribution.md`

## Context

The `omnifs` CLI is the single user-facing surface of the product: the first thing a user types after `npm install -g`, and the demo path the project leads with at launch. Daemon-internal verbs sit alongside daily-driver verbs and benefit from an explicit information architecture; render polish, completions, and config-file resolution earn their place at the same time.

This document records the decisions that shape the CLI surface, the path and config story, and the rendering stack. Implementation details (file layouts, exact code shapes) belong with the code; what lives here is rationale that should survive a refactor.

## Decisions

The persona pick (solo dev in a terminal) and the surface shape (flat verbs, hidden internals) are the upstream anchors; every other decision flows from those two.

| # | Branch | Decision | Why |
|---|---|---|---|
| 1 | Primary persona | Solo dev in a terminal | Optimize human polish first; machine output (`--json`) follows. |
| 2 | Surface shape | Flat verbs, hide internals under `omnifs debug` | Daily-driver muscle memory beats taxonomic purity. |
| 3 | Directory layout | Unified `~/.omnifs/{config,data,cache}` on every platform, with `OMNIFS_HOME` as the single env override | One layout to learn, easy to back up or delete; users who need a different root can move the whole layout without splitting policy across aliases. |
| 4 | macOS host-side file ops | Defer (`omnifs shell` is the access path) | Persona doesn't need it day one; macFUSE is a separate launch. |
| 5 | Render stack | `anstream` + `owo-colors` + `indicatif` + `comfy-table` + `inquire` | Idiomatic, composes well, respects `NO_COLOR`. |
| 6 | Tracing default | Silent; `-v` = INFO, `-vv` = DEBUG + span events; always stderr | The CLI is a terminal verb, not a service log. `RUST_LOG` overrides. |
| 7 | `init` args | `omnifs init <provider> [--as <name>]` | Positional means one thing; `--as` reads better than `--name`. |
| 8 | `--account` | Delete from every auth subcommand | A hidden flag for an undesigned feature is worse than no flag. |
| 9 | Token input | `--token -` (stdin) and `--token-env <VAR>` only | Two forms cover CI and pipes; `--token VALUE` leaks to shell history. |
| 10 | `--json` scope | `status` and `auth status` only | The runtime verifier needs a typed status payload; broader JSON locks in too much schema. |
| 11 | Config file | Global `config.toml`, precedence flag > env > file > default | Cuts repeated flags off every `up` invocation; ships dotfile-friendly. |
| 12 | `doctor` | Environment + auth, ~10 probes, no auto-fix | Diagnoses the failure modes our support replies already cover. |
| 13 | `ps` vs `status` | Single `status`, `--verbose` reveals provider runtime detail | One source of truth; `ps` would duplicate. |
| 14 | Auxiliary verbs | Only `mounts rm <name>` | Status lists, init picks; only removal needs its own verb. |
| 15 | Error codes | Free-form `anyhow` plus a `Try:` recovery block | A numbered taxonomy locks in too early. |
| 16 | Completions | bash, zsh, fish via `clap_complete` | Covers the long tail of solo-dev terminals. |
| 17 | Context detection | Env vars (`GITHUB_TOKEN` etc.); GitHub-specific `gh auth status` probe with scope warning, default No | Cheap delight; safety rails on the gh path. |
| 18 | Device flow | `arboard` auto-copy + countdown spinner + auto-open `verification_uri_complete` | Highest-attention moment; polish pays for itself. |
| 19 | Debug verbs | Hide `mount-tree`, `auth-manifest` under `omnifs debug`; no "not yet implemented" surfaces | Debug is useful but not for `--help`. |
| 20 | `version` | One line by default; rich on `--verbose` | Matches `gh`, `docker`. |
| 21 | Telemetry | None | Solo-dev persona skews privacy-conscious; reversible. |

## Architecture

### Paths

A single `paths` module resolves every directory the CLI uses from one source. Resolution per directory: explicit CLI flag, then `OMNIFS_HOME` (fans out into all subdirs), then the default `~/.omnifs/{config.toml,credentials.json,mounts,providers,cache}` layout. The default is uniform across macOS and Linux; users who want a relocated layout set one root instead of coordinating multiple directory-specific aliases.

The resolved `credentials_file` is threaded through every site that opens the credential store. The host store, the `auth login` write path, and the `omnifs up` read path must point at the same file or `auth login` and `omnifs up` will silently disagree.

A `Paths::display` helper home-relativizes (`~/.omnifs/mounts`) for every user-visible path; it falls back to the full path when the home prefix cannot be cleanly stripped.

### Config file

A global `config.toml` under `config_dir` supplies defaults for the runtime image, container name, and `up` toggles. Precedence is flag > file > built-in default. Missing file is not an error; malformed file is.

`Paths` locates `config.toml`; the loaded config no longer changes the directory layout.

### Surface

Top-level verbs: `init`, `up`, `down`, `status`, `doctor`, `mounts`, `dev`, `auth`, `shell`, `logs`, `completions`, `version`. Diagnostic surfaces (`mount-tree`, `auth-manifest`) live under hidden `omnifs debug`. The runtime entry point is the separate `omnifsd` binary, which the container entrypoint invokes (see `docs/design/daemon-cli-split.md`).

### Tracing

Default is silent. `-v` sets INFO; `-vv` sets DEBUG and enables span open/close events. All tracing, warnings, and errors go to stderr. Command success output stays on stdout so pipes work. `RUST_LOG` overrides the level whenever set.

### Error wrapper

`anyhow::Error` carries an optional `Hint` payload via an extension trait. The top-level catch renders the cause chain followed by a `Try:` block with hints attached at the failing site. Hints land on the highest-traffic failure paths: Docker unreachable, FUSE timeout, OAuth login failure, missing credential, provider missing. Format mirrors `docker` and `gh`.

### Status

`omnifs status` is the readiness view. It renders a `comfy-table` card listing runtime, mount, cache, and per-mount auth state. `--verbose` appends provider runtime detail. `--json` emits a typed `StatusJson` with `providers` always present; the runtime gate in `omnifs up` parses this payload (no stdout grepping).

On the host, `runtime` reports `unknown` and users are pointed at `omnifs doctor` for container probing; runtime detail is only populated inside the container.

### Doctor

Ten probes in dependency order. `image_cached` depends on `docker_reachable`. `auth_ready` depends on `mount_configs_valid` and reuses the same loaders as `status`. `network` is best-effort and never red. Exit code: 0 if all green/skipped, 1 if any red, 2 if no red but any yellow.

### `mounts rm`

Validates the mount name through `mount_name::validate` before any path construction; otherwise `mounts rm "../providers/foo"` could escape the mounts dir. Offers credential cleanup in the same flow because `auth logout` derives its credential key from the mount config file, which `rm` just deleted. Credentials with `ConfiguredExternally` source (`token_env`, `token_file`) are reported as unchanged.

### Token input

`auth import` and `init` accept `--token -` (stdin) and `--token-env VAR`. `--token <value>` is rejected (keeps secrets out of shell history). A `TokenSource` enum (`Stdin | Env | Interactive`) carries the source through the call chain so the read site is uniform.

### Init context detection

Before starting OAuth or prompting for a token, `init` probes per-provider sources: env vars (`GITHUB_TOKEN`, `LINEAR_API_KEY`, etc.) and, for GitHub specifically, `gh auth status`. The default is No (start OAuth); the `gh` path requires explicit `y` and surfaces the scope warning. The env-var path also requires `y` but suppresses the scope text (opaque source).

## Out of scope

The following are explicitly not part of this design and should not be folded in without a separate decision:

- Host-side file ops (`omnifs cat`, `omnifs path`, `omnifs open`). Deferred to a separate macOS-UX initiative.
- Native macOS FUSE mount via macFUSE.
- Multi-account support and the `--account` flag.
- Telemetry, crash reporting, usage analytics.
- Numbered stable error codes.
- A `mounts ls` / `mounts show` listing verb (status covers it).
- A `providers ls` / `providers info` verb (init picker covers it).
- Renaming `up`/`down`/`shell` to product-specific verbs.
- Per-directory `omnifs.toml` lookup walking from cwd.

## Open questions

- **`gh auth status` parsing fragility.** The output format is not contractually stable. If `gh` ships v3 and changes the scope line, the parser breaks. Acceptable risk; the fallback (start OAuth) is good. Worth a TODO at the parse site.
