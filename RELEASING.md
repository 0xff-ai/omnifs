# Releasing omnifs

How omnifs ships: conventional commits drive an always-open release PR; merging it tags and ships. Three workflows (`ci.yml`, `release-pr.yml`, `release.yml`), git-cliff for the changelog, no application recompile at ship time.

## What happens (end to end)

| Phase | Who / what | What runs | Outcome |
|-------|------------|-----------|---------|
| 1. Development | You + feature PRs (conventional commits) | CI verify | Work lands on `main` |
| 2. Standing release PR | `release-pr.yml`, every push to `main` | git-cliff next version + changelog; `cargo set-version`; `just npm sync` | An open `chore(release): vX.Y.Z` PR, always current |
| 3. Ship it | You merge the release PR | `release-pr.yml` tags the merge commit `vX.Y.Z` | `main` carries the bump; tag created |
| 4. Build | **CI** on the merge commit | native CLI archives + runtime image | four `omnifs-cli-*` tarballs, `sha-<commit>` image |
| 5. Publish | **`release.yml`** after green CI | GitHub Release + assets → GHCR promote → npm | GitHub Release `vX.Y.Z`, GHCR tags, npm `@0xff-ai/omnifs@X.Y.Z` |

Phase 5 recompiles nothing: `release.yml` downloads the artifacts from the CI run for the tagged commit.

```text
conventional-commit PRs ──► merge to main
        │
release-pr.yml ──► standing "chore(release): vX.Y.Z" PR (version + changelog + npm sync), refreshed each push
        │
you merge it ──► release-pr.yml tags vX.Y.Z
        │
CI builds artifacts ──► release.yml (after CI, gated on the tag): GH release → promote → npm
```

## The release coordinator (`release-pr.yml`)

On every push to `main`, the workflow does one of:

- **Accumulating.** Computes the next version from conventional commits since the last tag (git-cliff), rebuilds the `release-pr` branch with `cargo set-version`, `cargo update`, `just npm sync`, and a regenerated changelog, then force-updates the standing PR.
- **Merged.** When the release PR has landed (the workspace version is ahead of the last tag), it creates the `vX.Y.Z` tag, which `release.yml` ships off after CI.

The PR is the always-current preview of the next release. **Edit the changelog in the PR before merging** to polish wording; the version commit and PR body are yours to adjust. The PR itself does not run CI (it is created by `GITHUB_TOKEN`); validation runs on merge to `main`, and `release.yml` is gated on that CI succeeding.

## Changelog

git-cliff generates the pending changelog section from conventional commits (`cliff.toml`). Entries are grouped by product area (Providers & projected paths, Runtime & mounts, CLI & workflow, Caching & performance, Auth & credentials, Packaging & release) derived from the commit scope, and only user-facing types appear — `feat`, `fix`, `perf`; `refactor`/`docs`/`chore`/`ci`/`test`/`style` are filtered out. Curated released history (the `## [X.Y.Z]` sections already in `CHANGELOG.md`) is preserved; only the pending section is regenerated.

The **commit message is the changelog source**: write `feat(cli): ...`, `fix(mount): ...`, `feat(sdk)!: ...` (`!` marks a breaking change). An unscoped or unrecognized scope lands under "Other"; tighten the scope to place it. Polish the rendered entries in the release PR when you want prose over commit subjects.

## Versioning

The next version is computed from conventional commits since the last tag: `feat` → minor, `fix` → patch, breaking (`!`) → minor (omnifs is pre-1.0; `breaking_always_bump_major = false` in `cliff.toml`). To cut `1.0.0`, flip that flag. Prereleases are not produced automatically; to cut one (e.g. `0.3.0-rc.1`), set the version in the release PR by hand — `release.yml` detects the hyphen and marks the GitHub Release `prerelease=true, make_latest=false`, publishes npm with dist-tag `dev`, and still tags GHCR with `X.Y.Z` and `vX.Y.Z`.

## Workflows

| Workflow | Trigger | Role |
|----------|---------|------|
| `ci.yml` | push / PR to `main` | preflight, host/WASM verification, and on `main`: Linux + Darwin CLI archives, runtime images, smoke, the `sha-<commit>` manifest |
| `release-pr.yml` | push to `main` | maintain the standing release PR; tag `vX.Y.Z` when it merges |
| `release.yml` | `workflow_run` after successful CI on `main` | if a `v*` tag points at the built commit: GitHub Release + assets → GHCR promote → npm; platform npm packages staged from `npm/platforms.json` |

## Maintainer commands

The `just` surface for release-adjacent tasks is npm-only; the release itself is driven by merging the standing PR, not a local command.

| Subcommand | When | What it does |
|------------|------|----------------|
| **`just npm sync`** | CI before npm publish; optional locally | Set all `npm/**/package.json` versions from the Cargo workspace version through npm workspace-aware `npm pkg set` |
| **`just npm validate`** | preflight, ship | Cross-check `platforms.json`, the platform package.json manifests, and the inlined `resolve-binary.js` map (`cargo xtask npm validate`) |
| **`just npm pack`** | local verification, ship | Pack the root npm package locally |

Day-to-day dev uses the relevant CI-shaped just lanes, `just providers build`, and `omnifs dev`.

## What gets released

- **CLI**: `omnifs-cli-linux-*.tar.xz` from `cargo-zigbuild` with glibc 2.17, and `omnifs-cli-darwin-*.tar.xz` cross-linked from Linux through the pinned `rust-cross/cargo-zigbuild` container. These binaries embed the compressed provider/tool WASM bundle and unpack it into `OMNIFS_HOME/providers`.
- **Runtime**: `ghcr.io/0xff-ai/omnifs:<version>` promoted from `sha-<commit>` (also `v<version>` on GHCR; the CLI default uses the unprefixed tag). The runtime image stages the same Linux CLI binary.
- **npm**: `@0xff-ai/omnifs` + four platform packages.

## npm platform catalog

`npm/platforms.json` is the single source of truth for platform npm packages. Each entry defines the platform package name, Rust target triple, and npm `os`/`cpu` metadata. The Release workflow reads this file with `jq` while staging the four platform packages, so do not hand-maintain a second npm publishing matrix in `.github/workflows/release.yml`.

`npm/package.json` declares the private npm workspace that contains the root CLI package and the platform packages. `just npm sync` updates package versions by calling workspace-aware `npm pkg set`, not by reserializing JSON. This keeps package manifests in their existing order while still syncing the root package, platform packages, and root `optionalDependencies` to the Cargo workspace version. The repo-specific validation policy lives in `cargo xtask` (`crates/xtask/src/npm.rs`).

The bin shim at `npm/omnifs/bin/omnifs.js` and its `scripts/resolve-binary.js` helper must work entirely from files inside the `@0xff-ai/omnifs` package directory. `npm/platforms.json` lives at the workspace root and is **not** included in the published tarball; if `resolve-binary.js` ever needs that data it must be inlined, with `just npm validate` cross-checking against `npm/platforms.json`.

## Version coupling

For release `X.Y.Z`, the npm package version, the CLI `CARGO_PKG_VERSION` / `omnifs --version`, and the default runtime image tag all share the **same unprefixed semver** (`0.3.0`, not `v0.3.0`):

- npm: `@0xff-ai/omnifs@X.Y.Z` and matching `@0xff-ai/omnifs-cli-*` optional dependencies
- CLI default image: `ghcr.io/0xff-ai/omnifs:X.Y.Z` (`crates/omnifs-cli/src/session.rs`)
- Git tag / GitHub Release name: `vX.Y.Z` (the `v` prefix is used only here)
- GHCR promote publishes both `X.Y.Z` and `vX.Y.Z`; the CLI default uses the unprefixed tag

`cargo set-version` owns the bump: it sets `[workspace.package].version`, every crate that inherits it, and the workspace path-dependency requirements. Do not bump npm/Cargo versions outside the release PR, and do not change the embedded default image ref without going through a full release.

## Step-by-step (maintainer)

### 1. Land work on `main`

Merge feature PRs with conventional-commit messages (`type(scope): description`). The scope and type decide the changelog area and the version bump. Green `main` CI publishes `sha-<commit>`.

### 2. Verify the npm package locally (mandatory)

The publish pipeline cannot catch install-time failures, so verify before merging the release PR. Pack the root package as it would publish, install it into a scratch prefix both with and without scripts, and run the bin shim each time:

```bash
# 1. Pack the root npm package as it would publish.
scratch="$(mktemp -d)"; cd "$scratch"
npm pack "$OLDPWD/npm/omnifs"

# 2. Install the tarball into a scratch prefix, both with and without scripts.
prefix="$scratch/prefix"; mkdir -p "$prefix"
npm install --ignore-scripts --prefix "$prefix" "$scratch"/0xff-ai-omnifs-*.tgz
node "$prefix/node_modules/@0xff-ai/omnifs/bin/omnifs.js" --version    # must succeed

npm install --prefix "$prefix" "$scratch"/0xff-ai-omnifs-*.tgz         # postinstall path
node "$prefix/node_modules/@0xff-ai/omnifs/bin/omnifs.js" --version    # must succeed
```

If either invocation fails (MODULE_NOT_FOUND, missing platform binary, postinstall crash), the published package will fail the same way for every user. Fix before releasing.

### 3. Merge the standing release PR

Find the open `chore(release): vX.Y.Z` PR that `release-pr.yml` keeps current. Review the version bump and the regenerated changelog; edit the changelog in the PR if you want to polish wording or override the version (e.g. a prerelease). Merge it.

### 4. Wait for ship (automatic)

1. **`release-pr.yml`** on the merge commit tags `vX.Y.Z`.
2. **CI** on the merge commit: the artifact factory must go green.
3. **`release.yml`** fires after that CI, sees the tag on the commit, and ships. Watch **Actions → Release** for plan / github-release / promote / npm.

Re-run a failed **Release** job after fixing CI; do not re-run compile steps in ship. After publish, verify:

```bash
gh release view vX.Y.Z --json isPrerelease,assets
npm view @0xff-ai/omnifs --json | jq '.["dist-tags"]'
docker buildx imagetools inspect ghcr.io/0xff-ai/omnifs:X.Y.Z
```

## Prerequisites and secrets

| Secret | Used for |
|--------|----------|
| `GITHUB_TOKEN` | Release PR, tag, releases, artifacts, GHCR |
| `NPM_TOKEN` | npm publish; Automation type, bypasses 2FA, passed to the npm-platforms and npm-root jobs as `NODE_AUTH_TOKEN` |

Migrate `NPM_TOKEN` to npm Trusted Publishers (OIDC) per package after the first publish to remove the long-lived secret.

Gates that must be in place:

- `id-token: write` on the publish jobs (already set) for `--provenance`.
- The `release` label must exist on the GitHub repo; `release-pr.yml` attaches it to the release PR.
- `release-pr.yml` installs `git-cliff` and `cargo-edit` (`cargo set-version`) via `taiki-e/install-action`.

## What not to do

- Manual `git tag` / `git push --tags` (`release-pr.yml` owns tagging)
- Version bumps outside the release PR
- Rebuild image/WASM/CLI during ship

## Troubleshooting

| Problem | Fix |
|---------|-----|
| No release PR appears | `release-pr.yml` only opens one when there are releasable (`feat`/`fix`/`perf`) commits since the last tag |
| Wrong version computed | Check the conventional-commit types since the last tag; a stray `feat`/`!` changes the bump. Override by editing the version in the PR |
| Release workflow did not run | CI must succeed on the `main` push first |
| Ship ran but skipped | `release.yml` only ships when a `v*` tag points at the built commit; confirm `release-pr.yml` tagged the merge |
| Missing GH assets | CI must upload four `omnifs-cli-*` archives; re-run CI then Release |
| npm failed | Check the **promote** job; npm needs the GHCR tag + CI CLI artifacts |

## Configuration reference

| Path | Purpose |
|------|---------|
| `justfile`, `just/` | Maintainer command surface used locally and in CI |
| `cliff.toml` | git-cliff config: changelog areas, filters, and the version-bump policy |
| `crates/xtask` | repo-specific npm package validation (run via `cargo xtask npm validate`) |
| `npm/package.json` | private npm workspace root for the CLI and platform packages |
| `just/providers.just` | WASI SDK install (`wasi-sdk` recipe) and provider/tool WASM builds |
| `scripts/ci/common.sh` | Repo-root discovery and `version_pin()`, the string-pin reader for `tools/versions.toml` |
| `npm/platforms.json` | Source of truth for npm platform packages |
| `tools/versions.toml` | Pinned Zig, cargo-zigbuild, WASI SDK, and cargo tool versions used by CI |
| `.github/actions/omnifs-just` | Installs the pinned `just` version in CI |
| `scripts/ci/build-linux-zigbuild.sh` | Native Linux CLI build helper for the glibc baseline |
| `scripts/ci/build-darwin-zigbuild.sh` | Linux-hosted Darwin cross-link helper |
| `scripts/ci/build-runtime-image.sh` | Runtime image assembly from prebuilt CLI and WASM artifacts |
| `.github/workflows/release-pr.yml` | Release coordinator: standing PR + tag |
| `.github/workflows/release.yml` | Post-CI ship |
| `scripts/ci/promote-image.sh` | `sha-*` → semver GHCR tags |

## Related docs

- `CHANGELOG.md`
- `AGENTS.md`: `omnifs dev`, validation lanes
