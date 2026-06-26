# Just target plan

Goal: reduce the flattened maintainer command namespace without changing what the commands do.

## Current shape

The root `justfile` imports recipe files, so every task is exposed as a root-level command. That makes names encode their namespace manually: `ci-host`, `ci-wasm`, `providers-build`, `release-cut`, `npm-validate`, and similar.

The repo already has natural task families:

- `ci`: CI lanes and artifact assembly.
- `providers`: WASM provider/tool setup, checks, builds, and validation.
- `npm`: package sync, validation, and local packing.
- `release`: release checking, planning, and cutting.
- `dev`: everyday contributor commands such as formatting, OpenAPI checks, docs checks, and runtime launch.

## Direction

Use native `just` modules for task families instead of encoding the family into every recipe name.

Target shape:

```bash
just ci host
just ci wasm
just ci build-linux x86_64-unknown-linux-gnu
just providers check
just providers build
just providers validate
just npm validate
just npm sync
just release check
just release cut
```

Keep root-level commands only where they are deliberately blessed shortcuts:

```bash
just check
just dev
just fmt
just docs-check
```

## Constraints

- Preserve behavior first. The first refactor should be a namespace move, not a semantic rewrite.
- Keep existing CI/release scripts authoritative. Recipes should keep delegating to `scripts/ci/*`, `scripts/npm.ts`, and `scripts/release.ts`.
- Hide helper recipes with `[private]` only after confirming CI does not call them directly.
- Avoid unstable or surprising `just` features for gates that protect pushes and releases.
- Do not add compatibility aliases for every old target. Keep only high-frequency shortcuts and update docs/workflows for the rest.

## Migration steps

1. Convert `justfile` from `import` to `mod` for `ci`, `providers`, `npm`, and `release`.
2. Rename recipes inside each module to remove the duplicated family prefix.
3. Add root aliases or thin wrappers only for `check`, `dev`, `fmt`, and `docs-check`.
4. Update GitHub workflows, release docs, contracts, and `AGENTS.md` references to the new command names.
5. Run `just --list --unsorted`, representative `just --dry-run` commands, `just docs-check`, and `just check`.

## Open details

- Confirm module recipe working directories. Module recipes must still run from the repository root.
- Decide whether `dev` remains a root recipe or becomes a module with a default recipe.
- Decide whether `ci host-build` and `ci host-test` stay public because workflows call them, or become private helpers behind `ci host`.
