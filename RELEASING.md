# Releasing omnifs

How omnifs ships: conventional commits drive an always-open release PR; merging it tags and ships. Three workflows (`ci.yml`, `release-pr.yml`, `release.yml`), git-cliff for the changelog, no application recompile at ship time.

## What happens (end to end)

| Phase | Who / what | What runs | Outcome |
|-------|------------|-----------|---------|
| 1. Development | You + feature PRs (conventional commits) | CI verify | Work lands on `main` |
| 2. Standing release PR | `release-pr.yml`, every push to `main` | git-cliff next version + changelog; `cargo set-version`; `just npm sync` | An open `chore(release): vX.Y.Z` PR, always current |
| 3. Ship it | You merge the release PR | `release-pr.yml` tags the merge commit `vX.Y.Z` | `main` carries the bump; tag created |
| 4. Build | **CI** on the merge commit | native CLI archives + frontend image + guest artifact | four `omnifs-cli-*` tarballs and `sha-<commit>` frontend/guest artifacts |
| 5. Publish | **`release.yml`** after green CI | GitHub Release + assets → GHCR promotions → npm | GitHub Release `vX.Y.Z`, frontend/guest GHCR tags, npm `@0xff-ai/omnifs@X.Y.Z` |

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
| `ci.yml` | push / PR to `main` | preflight, host/WASM verification, and on `main`: Linux + Darwin CLI archives, frontend images, guest artifact, smoke, and `sha-<commit>` manifests |
| `release-pr.yml` | push to `main` | maintain the standing release PR; tag `vX.Y.Z` when it merges |
| `release.yml` | `workflow_run` after successful CI on `main` | if a `v*` tag points at the built commit: GitHub Release + assets → GHCR promote → npm; platform npm packages staged from `npm/platform/*` |

## Maintainer commands

The `just` surface for release-adjacent tasks is npm-only; the release itself is driven by merging the standing PR, not a local command.

| Subcommand | When | What it does |
|------------|------|----------------|
| **`just npm sync`** | CI before npm publish; optional locally | Set all `npm/**/package.json` versions from the Cargo workspace version through npm workspace-aware `npm pkg set` |
| **`just npm pack`** | local verification, ship | Pack the root npm package locally |

Day-to-day dev uses the relevant CI-shaped just lanes, `just build providers`, and `just dev`.

## What gets released

- **CLI**: `omnifs-cli-linux-*.tar.xz` from `cargo-zigbuild` with glibc 2.17, and `omnifs-cli-darwin-*.tar.xz` cross-linked from Linux through the pinned `rust-cross/cargo-zigbuild` container. These binaries embed the compressed provider/tool WASM bundle and unpack it into `OMNIFS_HOME/providers`.
- **Frontend**: `ghcr.io/0xff-ai/omnifs-frontend:<version>` promoted from the multi-platform `sha-<commit>` manifest (also `v<version>` on GHCR). It contains only the credential-free `omnifs-fuse` frontend.
- **Guest**: `ghcr.io/0xff-ai/omnifs-guest:<version>` promoted from the arm64 `sha-<commit>` OCI artifact (also `v<version>` on GHCR). It is the compressed libkrun disk image, not a container image.
- **npm**: `@0xff-ai/omnifs` + four platform packages.

## npm platform packages

The package manifests under `npm/platform/*/package.json` are the single source of truth for platform npm packages. Each one defines the package name plus npm `os`/`cpu` metadata. The Release workflow loops over those platform package directories while staging the platform packages, so do not hand-maintain a second npm publishing matrix in `.github/workflows/release.yml`.

`npm/package.json` declares the private npm workspace that contains the root CLI package and the platform packages. `just npm sync` updates package versions by calling workspace-aware `npm pkg set`, using the platform package manifests to rebuild the root package's `optionalDependencies` at the Cargo workspace version.

The bin shim at `npm/omnifs/bin/omnifs.js` and its `scripts/resolve-binary.js` helper must work from files available in the installed npm package graph. The root package's `optionalDependencies` list points at the platform packages, and `resolve-binary.js` reads the installed platform package metadata to find the package matching `process.platform` and `process.arch`.

## Version coupling

For release `X.Y.Z`, the npm package version, the CLI `CARGO_PKG_VERSION` / `omnifs --version`, and the default frontend and guest tags all share the **same unprefixed semver** (`0.3.0`, not `v0.3.0`):

- npm: `@0xff-ai/omnifs@X.Y.Z` and matching `@0xff-ai/omnifs-cli-*` optional dependencies
- CLI default frontend: `ghcr.io/0xff-ai/omnifs-frontend:X.Y.Z` (`crates/omnifs-cli/src/frontend_container.rs`)
- CLI default guest: `ghcr.io/0xff-ai/omnifs-guest:X.Y.Z` (`crates/omnifs-cli/src/krunkit_backend.rs`)
- Git tag / GitHub Release name: `vX.Y.Z` (the `v` prefix is used only here)
- Both GHCR promotions publish `X.Y.Z` and `vX.Y.Z`; the CLI defaults use the unprefixed tags

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
docker buildx imagetools inspect ghcr.io/0xff-ai/omnifs-frontend:X.Y.Z
oras manifest fetch ghcr.io/0xff-ai/omnifs-guest:X.Y.Z >/dev/null
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
- Rebuild frontend/guest/WASM/CLI artifacts during ship

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
| `npm/package.json` | private npm workspace root for the CLI and platform packages |
| `justfile`, `scripts/ci/build-providers.sh` | Action-first build recipes, the pinned WASI SDK installer, and provider/tool WASM builds |
| `scripts/ci/common.sh` | Repo-root discovery shared by CI helper scripts |
| `.github/actions/omnifs-just` | Installs the pinned `just` version in CI |
| `scripts/ci/build-linux-zigbuild.sh` | Native Linux CLI build helper for the glibc baseline |
| `scripts/ci/build-darwin-zigbuild.sh` | Linux-hosted Darwin cross-link helper |
| `scripts/ci/build-frontend-image.sh` | Frontend image assembly from a prebuilt CLI binary |
| `.github/workflows/release-pr.yml` | Release coordinator: standing PR + tag |
| `.github/workflows/release.yml` | Post-CI ship |
| `scripts/ci/promote-image.sh` | `sha-*` → semver GHCR tags |

## Related docs

- `CHANGELOG.md`
- `AGENTS.md`: `just dev`, validation lanes
