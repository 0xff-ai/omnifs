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

Prerelease versions (anything containing `-`, e.g. `0.2.0-dev.0`) are auto-detected: the GitHub Release is marked `prerelease=true, make_latest=false`, npm publishes with dist-tag `dev`, and GHCR still gets both `X.Y.Z` and `vX.Y.Z` tags.

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
- **Runtime**: `ghcr.io/0xff-ai/omnifs:<version>` promoted from `sha-<commit>` (also `v<version>` on GHCR; CLI default uses unprefixed tag)
- **npm**: `@0xff-ai/omnifs` + four platform packages

## npm platform catalog

`npm/platforms.json` is the single source of truth for platform npm packages. Each entry defines the platform package name, Rust target triple, and npm `os`/`cpu` metadata. The Release workflow reads this file with `jq` while staging the four platform packages, so do not hand-maintain a second npm publishing matrix in `.github/workflows/release.yml`.

`just npm-sync` updates package versions by calling `npm pkg set`, not by reserializing JSON. This keeps package manifests in their existing order while still syncing the root package, platform packages, and root `optionalDependencies` to the workspace version. The npm policy implementation lives in `scripts/lib/npm-workspace.ts`, consumed by both `scripts/npm.ts` and the Release workflow.

The bin shim at `npm/omnifs/bin/omnifs.js` and its `scripts/resolve-binary.js` helper must work entirely from files inside the `@0xff-ai/omnifs` package directory. `npm/platforms.json` lives at the workspace root and is **not** included in the published tarball; if `resolve-binary.js` ever needs that data it must be inlined, with `just npm-validate` cross-checking against `npm/platforms.json`. The same rule applies to any future runtime helper added to the published package.

## Version coupling

For release `X.Y.Z`, the npm package version, the CLI `CARGO_PKG_VERSION` / `omnifs --version`, and the default runtime image tag all share the **same unprefixed semver** (`0.2.0`, not `v0.2.0`):

- npm: `@0xff-ai/omnifs@X.Y.Z` and matching `@0xff-ai/omnifs-cli-*` optional dependencies
- CLI default image: `ghcr.io/0xff-ai/omnifs:X.Y.Z` (`crates/omnifs-cli/src/session.rs`)
- Git tag / GitHub Release name: `vX.Y.Z` (the `v` prefix is used only here)
- GHCR promote publishes both `X.Y.Z` and `vX.Y.Z`; the CLI default uses the unprefixed tag

npm installs the native CLI binary only. Docker is pulled on `omnifs up`, not at `npm install`.

Do not bump npm/Cargo versions outside `just release-cut`. Do not change the embedded default image ref without going through a full release.

## Step-by-step (maintainer)

### 1. Land work on `main`

Merge features with changelog updates. Green `main` CI publishes `sha-<commit>` and (on `main` only) release artifacts.

### 2. Verify the npm package locally (mandatory)

The publish pipeline cannot catch install-time failures, so verify before cutting. Pack the root package as it would publish, install it into a scratch prefix both with and without scripts, and run the bin shim each time:

```bash
# 1. Pack the root npm package as it would publish.
scratch="$(mktemp -d)"; cd "$scratch"
npm pack /Users/raul/W/omnifs/npm/omnifs

# 2. Install the tarball into a scratch prefix, both with and without scripts.
prefix="$scratch/prefix"; mkdir -p "$prefix"
npm install --ignore-scripts --prefix "$prefix" "$scratch"/0xff-ai-omnifs-*.tgz
node "$prefix/node_modules/@0xff-ai/omnifs/bin/omnifs.js" --version    # must succeed

npm install --prefix "$prefix" "$scratch"/0xff-ai-omnifs-*.tgz         # postinstall path
node "$prefix/node_modules/@0xff-ai/omnifs/bin/omnifs.js" --version    # must succeed
```

If either invocation fails (MODULE_NOT_FOUND, missing platform binary, postinstall crash), the published package will fail the same way for every user. Fix before cutting.

### 3. Cut the release PR

Prerequisites: clean `main` = `origin/main`, `gh` auth, `[Unreleased]` filled, `just`, and `cargo install cargo-edit` for `cargo set-version`.

```bash
just release-cut
```

`just release-cut` creates `release/vX.Y.Z`, bumps workspace + npm, finalizes CHANGELOG, commits, pushes, opens PR.

Optional before cut: `just release-prompt` → draft notes → commit on `main` → then `just release-cut`.

### 4. Merge the release PR

Wait for PR CI (including `just release-check`). Merge via squash and delete branch.

### 5. Wait for ship (automatic)

1. **CI** on merge commit: factory must go green.
2. **Release** workflow: `workflow_run` fires only after that CI succeeds.
3. Watch **Actions → Release** for plan / github-release / promote / npm.

Re-run a failed **Release** job after fixing CI; do not re-run compile steps in ship.

After publish, verify:

```bash
gh release view vX.Y.Z --json isPrerelease,assets
npm view @0xff-ai/omnifs --json | jq '.["dist-tags"]'    # dev tag should point at the new version for a prerelease
docker buildx imagetools inspect ghcr.io/0xff-ai/omnifs:X.Y.Z
```

## Prerequisites and secrets

| Secret | Used for |
|--------|----------|
| `GITHUB_TOKEN` | Releases, artifacts, GHCR |
| `NPM_TOKEN` | npm publish; Automation type, bypasses 2FA, passed to the npm-platforms and npm-root jobs as `NODE_AUTH_TOKEN` |

Migrate `NPM_TOKEN` to npm Trusted Publishers (OIDC) per package after the first publish to remove the long-lived secret.

Gates that must be in place before any cut:

- `id-token: write` permission on the publish jobs (already set) for `--provenance`.
- The `release` label must exist on the GitHub repo; `release.ts publishReleasePr` attaches it to the cut PR and fails if missing.
- `release-cut` requires `cargo set-version`; install with `cargo install cargo-edit` if missing.

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

### Failure modes seen in practice

Record new ones here when they happen so the next cut avoids them.

- **Race against main's CI**: when a PR merges and you immediately `release-cut`, the resulting `release/vX.Y.Z` PR starts CI before main's post-merge CI has saved its caches under `refs/heads/main`. PR CI then runs cold on lanes like `cli (linux-x64, darwin)`. Mitigation: wait for the prior main CI to complete before cutting, or accept one cold cycle.
- **Branch name collision**: if a non-versioned branch named `release` exists (locally or remotely), git refuses to create `release/vX.Y.Z` (`cannot lock ref ... 'refs/heads/release' exists`). Delete or rename the conflicting branch first. The PR branch from the last release cleanup is the usual culprit; delete the remote (`gh pr merge --delete-branch` or `git push origin --delete release`) and `git remote prune origin` locally.

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
