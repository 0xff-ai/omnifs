# Releasing omnifs

How omnifs ships: one maintainer command surface (`just`), two workflows (`ci.yml` + `release.yml`), no compile at ship time.

## What happens (end to end)

| Phase | Who / what | What runs | Outcome |
|-------|------------|-----------|---------|
| 1. Development | You + feature PRs | CI `verify` + `release-check` → `just release-check` | `[Unreleased]` updated (or `no-changelog` label) |
| 2. Cut release | You on `origin/main` | `just release-cut` | Branch `release/vX.Y.Z`, version bump, changelog finalized, release PR opened |
| 3. Review | You + release PR CI | CI verify; `just release-check` | Release PR shape validated |
| 4. Merge | Merge to `main` | **CI** factory | `omnifs-wasm`, four `omnifs-cli-*` tarballs, runtime image `sha-<commit>` |
| 5. Ship | **Release** workflow after green CI | `just release-plan-json` → softprops → promote → npm | GitHub Release `vX.Y.Z`, GHCR tags, npm `@0xff-ai/omnifs@X.Y.Z` |

Nothing in phase 5 recompiles. Release downloads artifacts from the CI run that triggered it (`workflow_run`).

```text
feature PR ──► ci release-check
                    │
main + notes ──► release cut (local) ──► release/v* PR ──► release check
                    │
              merge to main ──► CI factory (artifacts + image)
                    │
              CI success ──► Release workflow (plan → GH release → promote → npm)
```

## Maintainer commands

Maintainer commands are exposed through the root **`justfile`**. Run `just` to list the grouped command surface. The release and npm recipes call policy-heavy Bun scripts at `scripts/{npm,release}.ts`, which dispatch to `scripts/lib/`.

| Subcommand | When | What it does |
|------------|------|----------------|
| **`just release-cut`** | Local, on clean `main` | Bump versions, finalize CHANGELOG, commit on `release/vX.Y.Z`, push, open PR |
| **`just release-cut-local`** | Local, on clean `main` | Prepare the release branch without pushing |
| **`just release-check`** | Every PR (CI) | On `release/*` branches: validate release PR. Else: validate `[Unreleased]` vs base |
| **`just release-plan-json`** | CI only, after green `main` | If workspace version > latest tag: emit ship metadata; else no-op |
| **`just npm-sync`** | CI before npm publish; optional locally | Set all `npm/**/package.json` versions from workspace (or `version`) through `npm pkg set` |
| **`just npm-validate`** | `just check`, release PR, ship | Cross-check `platforms.json`, package.json, and `dist-workspace.toml` |

Optional (not part of the ship path):

```bash
just release-prompt   # LLM helper: commit range since last tag
```

Examples:

```bash
# Cut a release (interactive patch bump)
git fetch origin && git checkout main && git reset --hard origin/main
just release-cut

# Pin version or prepare without pushing
just release-cut 0.2.0
just release-cut-local 0.2.0

# Match CI changelog check on a feature branch
just release-check origin/main HEAD
```

Day-to-day dev uses `just check`, `just providers-build`, and `omnifs dev`.

## Workflows

| Workflow | Trigger | Role |
|----------|---------|------|
| `ci.yml` | push / PR to `main` | Fast preflight, native host/WASM verification, and on `main`/`ci-full`: Linux + Darwin CLI archives, runtime images, smoke, and the `sha-<commit>` manifest |
| `release.yml` | `workflow_run` after successful **CI** on `main` `push` | `just release-plan-json` → GitHub Release + assets → GHCR promote → npm; platform npm packages are staged from `npm/platforms.json` |

## Release authority

- **`just release-cut`**: version bump in the release PR only (Cargo, lockfile, npm, CHANGELOG).
- **`just release-plan-json`**: ship gating (version vs latest tag, changelog/npm validation, release notes for softprops).
- **CI**: single factory; builds all binaries and images.
- **`release.yml`**: attach CI artifacts, promote `sha-*` → semver tags, publish npm.

## Branch hygiene

**Always treat `origin/main` as the release base**, not local `main`.

```bash
git fetch origin
git checkout main
git reset --hard origin/main   # only when local main has diverged
```

When reviewing a release branch: `git diff origin/main --stat`, not `git diff main` if local `main` drifted.

## Changelog policy

[Keep a Changelog](https://keepachangelog.com/). Humans or LLMs write prose; automation never invents entries.

- Feature PRs: update `## [Unreleased]` (or label **`no-changelog`** for exempt chores).
- Release PR: `just release-cut` moves `[Unreleased]` into `## [X.Y.Z] - date` and leaves an empty `[Unreleased]`.

## What gets released

- **CLI**: `omnifs-cli-linux-*.tar.xz` from `cargo-zigbuild` with glibc 2.17, and `omnifs-cli-darwin-*.tar.xz` cross-linked from Linux through the pinned `rust-cross/cargo-zigbuild` container
- **WASM**: `omnifs-wasm` artifact (`omnifs_provider_*.wasm`, `omnifs_tool_*.wasm`)
- **Runtime**: `ghcr.io/raulk/omnifs:<version>` promoted from `sha-<commit>` (also `v<version>` on GHCR; CLI default uses unprefixed tag)
- **npm**: `@0xff-ai/omnifs` + four platform packages

## npm platform catalog

`npm/platforms.json` is the source of truth for platform npm packages. Each entry defines the platform package name, Rust target triple, and npm `os`/`cpu` metadata. The Release workflow reads this file with `jq` while staging platform packages, so do not copy the npm publishing matrix into `.github/workflows/release.yml`.

`just npm-sync` updates package versions by calling `npm pkg set`, not by reserializing JSON. This keeps package manifests in their existing order while still syncing the root package, platform packages, and root `optionalDependencies` to the workspace version.

## Version coupling

For release `X.Y.Z`, npm, `omnifs --version`, and default image `ghcr.io/raulk/omnifs:X.Y.Z` share the **same unprefixed semver**. Git tag / GitHub Release name use **`vX.Y.Z`**.

Do not bump versions outside `just release-cut`. Do not change the embedded default image ref without going through a full release.

## Step-by-step (maintainer)

### 1. Land work on `main`

Merge features with changelog updates. Green `main` CI publishes `sha-<commit>` and (on `main` only) release artifacts.

### 2. Cut the release PR

Prerequisites: clean `main` = `origin/main`, `gh` auth, `[Unreleased]` filled, `just`, and `cargo install cargo-edit` for `cargo set-version`.

```bash
just release-cut
```

`just release-cut` creates `release/vX.Y.Z`, bumps workspace + npm, finalizes CHANGELOG, commits, pushes, opens PR.

Optional before cut: `just release-prompt` → draft notes → commit on `main` → then `just release-cut`.

### 3. Merge the release PR

Wait for PR CI (including `just release-check`). Merge to `main`.

### 4. Wait for ship (automatic)

1. **CI** on merge commit: factory must go green.
2. **Release** workflow: starts only after that CI succeeds.
3. Watch **Actions → Release** for plan / github-release / promote / npm.

Re-run a failed **Release** job after fixing CI; do not re-run compile steps in ship.

## Prerequisites and secrets

| Secret | Used for |
|--------|----------|
| `GITHUB_TOKEN` | Releases, artifacts, GHCR |
| npm Trusted Publishing | npm publish via GitHub OIDC |

Local **`just release-cut`**: requires `cargo-edit` (`cargo set-version`) so Cargo owns workspace version and path dependency updates.

## What not to do

- Manual `git tag` / `git push --tags`
- Version bumps outside `just release-cut`
- Rebuild image/WASM/CLI during ship
- `prepare` from a stale local `main`

## Troubleshooting

| Problem | Fix |
|---------|-----|
| Feature PR changelog check failed | Update `[Unreleased]` or add `no-changelog` |
| Release PR check failed | Ensure `## [version]` exists, `[Unreleased]` empty, versions synced |
| Release workflow did not run | CI must succeed on `main` push first |
| Missing GH assets | CI must upload `omnifs-wasm` + four `omnifs-cli-*`; re-run CI then Release |
| npm failed | Check **promote** job; npm needs GHCR tag + CI CLI artifacts |
| No ship after merge | `just release-plan-json` no-ops if version ≤ latest tag; run `just release-cut` again |

## Configuration reference

| Path | Purpose |
|------|---------|
| `justfile`, `just/` | Maintainer command surface used locally and in CI |
| `scripts/npm.ts` | npm platform catalog, package sync, and package validation (logic in `scripts/lib/npm-workspace.ts`) |
| `scripts/release.ts` | release cut, release check, release plan, and release-note prompt (logic in `scripts/lib/release-workflow.ts`) |
| `scripts/toolchain/wasi-env.ts` | WASI SDK bootstrap and env export for provider builds |
| `scripts/toolchain/versions.ts` | Read a scalar pin from `tools/versions.toml` |
| `npm/platforms.json` | Source of truth for npm platform packages |
| `tools/versions.toml` | Pinned Zig, cargo-zigbuild, WASI SDK, and cargo tool versions used by CI |
| `.github/actions/omnifs-just` | Installs the pinned `just` version in CI |
| `scripts/ci/build-linux-zigbuild.sh` | Native Linux CLI build helper for the glibc baseline |
| `scripts/ci/build-darwin-zigbuild.sh` | Linux-hosted Darwin cross-link helper |
| `scripts/ci/build-runtime-image.sh` | Runtime image assembly from prebuilt CLI and WASM artifacts |
| `.github/workflows/ci.yml` | Factory + `sha-*` image + PR release checks |
| `.github/workflows/release.yml` | Post-CI ship |
| `scripts/ci/promote-image.sh` | `sha-*` → semver GHCR tags |

## Related docs

- `CHANGELOG.md`
- `AGENTS.md`: `omnifs dev`, `just check`
