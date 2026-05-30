---
title: npm distribution
description: The npm package installs the native CLI only; platforms.json is the single source of truth; the bin shim must resolve from inside the published package.
---

omnifs is distributed to end users as an npm package. The package installs the
**native CLI binary only**. The Docker runtime image is pulled lazily on
`omnifs up`, never at `npm install` time.

:::note
npm install gives you the CLI. The runtime image is a separate artifact, pulled
on first `omnifs up`. This keeps `npm install` fast and offline-friendly.
:::

## Package layout

| Package | Role |
|---|---|
| `@0xff-ai/omnifs` | Root package. Bin shim + `resolve-binary.js`. Declares the four platform packages as `optionalDependencies`. |
| `@0xff-ai/omnifs-cli-linux-x64` | Linux x86_64 prebuilt CLI. |
| `@0xff-ai/omnifs-cli-linux-arm64` | Linux aarch64 prebuilt CLI. |
| `@0xff-ai/omnifs-cli-darwin-x64` | macOS x86_64 prebuilt CLI. |
| `@0xff-ai/omnifs-cli-darwin-arm64` | macOS aarch64 prebuilt CLI. |

Each platform package carries a single prebuilt CLI binary and declares its `os`
and `cpu`, so npm installs only the matching one. The root package depends on all
four as `optionalDependencies`; npm silently skips the non-matching platforms.

## Single source of truth

`npm/platforms.json` at the workspace root is the single source of truth for:

- platform package names
- Rust target triples
- npm `os` / `cpu` metadata

```json
{
  "platforms": [
    { "name": "@0xff-ai/omnifs-cli-linux-x64",   "target": "x86_64-unknown-linux-gnu",  "os": "linux",  "cpu": "x64" },
    { "name": "@0xff-ai/omnifs-cli-linux-arm64",  "target": "aarch64-unknown-linux-gnu", "os": "linux",  "cpu": "arm64" },
    { "name": "@0xff-ai/omnifs-cli-darwin-x64",   "target": "x86_64-apple-darwin",       "os": "darwin", "cpu": "x64" },
    { "name": "@0xff-ai/omnifs-cli-darwin-arm64", "target": "aarch64-apple-darwin",      "os": "darwin", "cpu": "arm64" }
  ]
}
```

`release.yml` reads `platforms.json` directly when staging the four platform
packages.

:::caution
Do not hand-maintain a second publishing matrix in GitHub Actions. The npm policy
implementation lives in `scripts/lib/npm-workspace.ts`, consumed by both
`scripts/npm.ts` and the release workflow. Keep it in that one module.
:::

## The bin shim

`npm/omnifs/bin/omnifs.js` resolves and execs the platform binary. Together with
`scripts/resolve-binary.js`, it must work entirely from files **inside** the
published `@0xff-ai/omnifs` package directory.

:::danger
`npm/platforms.json` lives at the workspace root and is **not** included in the
published tarball. If `resolve-binary.js` ever needs that data, it must be
**inlined**, with `just npm-validate` cross-checking the inlined copy against
`npm/platforms.json`. The same rule applies to any future runtime helper added to
the published package.
:::

## Keeping versions in sync

`just npm-sync` updates package versions through `npm pkg set`, preserving
manifest order and formatting. It is invoked by `just release-cut`; do not bump
npm versions on their own. See [Version coupling](/releasing/version-coupling/).

```bash
just npm-sync       # bump root + platform package versions in lockstep
just npm-validate   # validate package metadata against npm/platforms.json
```

## Verify before publishing

The publish pipeline cannot catch install-time failures. Always pack and install
the tarball locally — with and without scripts — before cutting. See the
[Release process](/releasing/process/#local-verification-before-cutting).

## See also

- [Release process](/releasing/process/)
- [Version coupling](/releasing/version-coupling/)
- [Runtime image](/releasing/runtime-image/)
