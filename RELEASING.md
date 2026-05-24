# Releasing omnifs

This document describes how to cut a release. Maintainers use the internal `omnifs-release` CLI to prepare a release PR locally. Merging that PR creates the tag and GitHub Release and triggers the ship pipeline.

Release automation replaces the previous tag-triggered `release.yml` and separate `npm.yml` workflow with a maintainer-driven prepare flow. All version bumps, tag creation, and ship gating go through `omnifs-release` plus the workflows below.

## Overview

Three workflows cooperate:

| Layer | Workflow | Trigger | Responsibility |
|-------|----------|---------|----------------|
| CI | `.github/workflows/ci.yml` | push to `main`, PRs | lint, test, docker build (amd64 + arm64), smoke, push `ghcr.io/<repo>:sha-<commit>` |
| Changelog guard | `.github/workflows/changelog-check.yml` | PRs to `main` | require `[Unreleased]` updates on every PR unless labeled `no-changelog`; validate release PR shape |
| Release + ship | `.github/workflows/release-on-merge.yml` | push to `main` | when workspace version exceeds latest tag: create GitHub Release, call `ship-release.yml` |

```text
During development
  └─ edit CHANGELOG.md [Unreleased] on feature PRs

Cut release locally (from origin/main)
  └─ just prepare-release
  └─ optional: just release-prompt  (LLM draft from commit range)
  └─ review release PR, merge to main

Merge release PR to main
  └─ release-on-merge: gh release create vX.Y.Z (notes from CHANGELOG.md)
  └─ ship-release: resolve tag → commit SHA; dist/WASM/npm/promote all use that SHA
```

## Release authority

**`omnifs-release`** owns the version bump in the release PR, GitHub Release creation on merge, and validation checks. **ship-release** builds and publishes artifacts for the commit the tag points at, not necessarily `github.sha` from the workflow run.

After tagging, `omnifs-release create-github-release` verifies the tag points at the release commit and passes that SHA through dist, WASM build, and GHCR promotion (`sha-<commit>`).

## Branch hygiene

**Always treat `origin/main` as the release base**, not local `main`.

Local `main` can drift if you experimented with release or WIT vendoring work on that branch. Release PRs and `prepare` must start from the same commit GitHub uses:

```bash
git fetch origin
git checkout main
git reset --hard origin/main   # only when local main has diverged
```

When reviewing a release branch locally, diff against `origin/main`:

```bash
git diff origin/main --stat
git log origin/main..HEAD --oneline
```

Do **not** use `git diff main` if local `main` is ahead of `origin/main`; that shows unrelated deletions (for example vendored `crates/*/wit/*` files that exist only on a stale local branch).

## Changelog policy

`CHANGELOG.md` follows [Keep a Changelog](https://keepachangelog.com/). **Humans or LLMs write the prose**; release automation never auto-generates changelog entries.

Every PR to `main` must update `CHANGELOG.md` under `## [Unreleased]`. Add the **`no-changelog`** label to exempt chore-only PRs (docs typos, CI-only tweaks with no user-visible effect, etc.).

### During development

Add notes under `## [Unreleased]` as you land notable changes:

```markdown
## [Unreleased]

### Added

- Short description of the user-visible change.
```

Use the existing sections (`Added`, `Changed`, `Fixed`, etc.) and write for end users, not commit subjects.

Local check (matches CI; defaults to `origin/main`):

```bash
cargo run -p omnifs-release -- check changelog-pr --base origin/main --head HEAD
```

### Release PR shape

Release PRs use branch names `release/v*`. CI validates them separately:

```bash
just check-release
# equivalent:
cargo run -p omnifs-release -- check release-pr
```

Before merge, confirm:

- `## [X.Y.Z]` exists with release notes
- `## [Unreleased]` is present and empty
- npm and Cargo versions match the target version

## What gets released

Each release ships:

- **Host CLI** — cross-platform tarballs (`omnifs-cli-*`) built by [cargo-dist](https://github.com/axodotdev/cargo-dist) and uploaded to the GitHub Release
- **WASM providers** — `omnifs_provider_*.wasm` for built-in providers
- **Runtime image** — `ghcr.io/raulk/omnifs:<version>` and `ghcr.io/raulk/omnifs:v<version>`, promoted from the CI-built `sha-<commit>` image (not rebuilt on release)
- **npm packages** — `@0xff-ai/omnifs` plus four platform packages, staged from dist CLI archives

The CLI embeds the runtime image reference `ghcr.io/raulk/omnifs:<CARGO_PKG_VERSION>`. npm publish is gated on that image existing in GHCR before any package goes live.

## Version source of truth

- **Cargo:** `[workspace.package].version` in `Cargo.toml` (bumped in the release PR by `omnifs-release prepare`)
- **npm:** root and platform `package.json` files, synced by `omnifs-release prepare`
- **Git tag / GitHub Release:** `vX.Y.Z`, created by `gh release create` on merge

Internal workspace crates use path-only dependencies. We do not publish crates to crates.io.

### Version coupling (npm, CLI, runtime image)

For a release `X.Y.Z`, these identifiers use the **same semver string without a `v` prefix**:

| Surface | Example for 0.2.0 | Notes |
|---------|-------------------|-------|
| Cargo workspace | `0.2.0` | `[workspace.package].version` |
| npm root + platform packages | `0.2.0` | `@0xff-ai/omnifs`, `@0xff-ai/omnifs-cli-*` |
| CLI `omnifs --version` | `omnifs 0.2.0` | `CARGO_PKG_VERSION` at build time |
| Default runtime image | `ghcr.io/raulk/omnifs:0.2.0` | compile-time default in `crates/cli/src/session.rs` |

**Git and GitHub** use the conventional **`v` prefix** (`v0.2.0`) for tags and release names. That is separate from npm/Cargo semver and from the CLI's default image ref.

**GHCR** gets both tags on promote: `0.2.0` and `v0.2.0`, both pointing at the same digest as the CI `sha-<commit>` image. The CLI default pull uses the **unprefixed** tag only.

```text
npm install @0xff-ai/omnifs@0.2.0
  └─ optional platform package @0.2.0 ships omnifs binary (0.2.0)

omnifs up
  └─ pulls ghcr.io/raulk/omnifs:0.2.0 by default (not :v0.2.0)
```

npm does **not** fetch Docker at install time. Ship CI verifies the promoted image exists before publishing npm packages so the default ref is live when users run `omnifs up`.

Overrides (`OMNIFS_IMAGE`, config `image`, `--image`) break this coupling intentionally; do not change defaults without updating all version surfaces above.

## Step-by-step release

### 1. Land changes on main

Merge feature PRs to `main` as usual. Keep `CHANGELOG.md [Unreleased]` up to date.

CI must pass on `main`. Each green CI run publishes `ghcr.io/raulk/omnifs:sha-<commit>` for that merge commit.

### 2. Prepare the release PR locally

```bash
git fetch origin
git checkout main
git reset --hard origin/main
just prepare-release
# or pin the version:
just prepare-release -- --version 0.2.0
# dry run (commit locally, no push/PR):
just prepare-release -- --no-push
```

`prepare`:

1. Creates branch `release/vX.Y.Z`
2. Bumps workspace version and refreshes `Cargo.lock` via `cargo update --workspace`
3. Syncs npm package versions (including root `optionalDependencies`)
4. Moves `[Unreleased]` into `## [X.Y.Z] - YYYY-MM-DD`
5. Restores an empty `## [Unreleased]`
6. Commits, pushes, and opens a PR labeled `release` (unless `--no-push`)

Optional LLM-assisted changelog **before** running `prepare`:

```bash
just release-prompt > /tmp/release-notes-prompt.md
# paste into your editor/LLM; add the output under ## [Unreleased] on main, commit, then prepare
```

The prompt includes only the commit range since the last tag; inspect git locally or in your editor to draft notes.

Review the opened PR. Edit changelog wording on the release branch if needed.

### 3. Merge the release PR

Merge when green. The next push to `main` triggers `.github/workflows/release-on-merge.yml`.

CI runs `cargo run -p omnifs-release -- check release-pr` on release PRs (includes `validate-platforms.mjs`) and again inside `ship-plan` before tagging.

### 4. Ship runs automatically

When the workspace version is greater than the latest semver tag:

1. **Ship plan** — `cargo run -p omnifs-release -- ship-plan`
2. **GitHub Release** — `cargo run -p omnifs-release -- create-github-release`
3. **Ship** (`.github/workflows/ship-release.yml`), in order:
   - **dist** — cargo-dist uploads CLI archives to the release
   - **WASM** — provider components uploaded to the same release
   - **promote** — waits for `sha-<commit>`, then promotes to `vX.Y.Z` and `X.Y.Z` on GHCR
   - **npm** — downloads dist archives, verifies CLI version and runtime image, publishes platform packages then the root package

Monitor progress under **Actions → Release on merge** on GitHub.

## Prerequisites and secrets

### CI image must exist for the release commit

Ship promotes `ghcr.io/<repo>:sha-<commit>` for the **tagged release commit**. CI must have published that `sha-*` image on `main`. If you merge the release PR before CI finishes for that commit, promotion waits for the `sha-*` tag to appear.

### GitHub secrets

| Secret | Used for |
|--------|----------|
| `GITHUB_TOKEN` | GitHub Release, dist upload, GHCR login, npm asset download |
| `NPM_TOKEN` | npm publish (`@0xff-ai/omnifs` and platform packages) |

### Local prepare prerequisites

- Clean working tree on `main` synced to `origin/main`
- `gh` authenticated with permission to open PRs
- `[Unreleased]` contains release notes

## What not to do

- Do not run `git tag` / `git push --tags` manually
- Do not bump `[workspace.package].version` outside a release PR
- Do not expect cargo-dist to create the GitHub Release or write release notes (upload-only)
- Do not rebuild the runtime docker image during release (promote-only)
- Do not prepare a release from a local `main` that is ahead of `origin/main`

## Troubleshooting

### Feature PR check failed: changelog

The PR did not update `CHANGELOG.md [Unreleased]`. Add user-facing notes, or apply the **`no-changelog`** label for exempt chore-only changes.

### Release PR check failed: changelog

Finalize the changelog on the release PR: add `## [X.Y.Z] - date`, move notes out of `[Unreleased]`, and leave `[Unreleased]` empty.

### Release PR diff shows unexpected WIT or manifest changes

You are probably diffing against stale local `main`. Run `git fetch origin && git diff origin/main --stat` on the release branch instead.

### Image promotion timed out

CI may still be running, or CI failed on the release commit. Check **Actions → CI** for that SHA. Fix CI on `main` and re-run the failed ship job, or cut a patch release after CI is green.

### npm publish failed: runtime image missing

Promotion did not complete before npm ran, or GHCR promotion failed. Check the **promote** job in ship-release. npm verifies `ghcr.io/raulk/omnifs:<version>` exists before publishing.

### Release did not ship after merge

`ship-plan` only ships when the workspace version is greater than the latest `v*.*.*` tag. If you merged a non-release PR, nothing happens. If the version was not bumped, run `prepare` again.

### Ship used the wrong commit

Verify the tagged commit with `git rev-parse vX.Y.Z^{commit}` and confirm the ship workflow's `release_commit_sha` matches.

## Configuration reference

| File | Purpose |
|------|---------|
| `crates/omnifs-release/` | Maintainer CLI: prepare, check, prompt, ship-plan |
| `dist-workspace.toml` | cargo-dist targets and `allow-dirty` for custom release.yml jobs |
| `scripts/promote-release-image.sh` | Promotes `sha-*` → semver tags on GHCR |
| `scripts/wait-for-ghcr-tag.sh` | Polls until a GHCR tag is readable |
| `npm/scripts/sync-version.mjs` | Syncs npm versions from Cargo workspace version (also used in ship CI) |
| `npm/scripts/validate-platforms.mjs` | Validates npm layout against `platforms.json` and ship-release matrix |

## Related docs

- `CHANGELOG.md` — release history and `[Unreleased]` draft
- `AGENTS.md` — contributor workflow (`omnifs dev`, validation commands)
- Provider READMEs — note that `.wasm` artifacts ship on each GitHub Release
