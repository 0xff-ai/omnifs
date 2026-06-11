# Crate structure

Status: proposed (not yet implemented)
Scope: the shared type, configuration, and provider-contract crates: the proposed split of `omnifs-mount-schema`, the new `omnifs-home` layout crate, the auth-type and `Spec`/`Resolved` consolidations, and the resulting dependency graph
Related: `docs/design/daemon-cli-split.md`

## Context

The daemon/CLI split (`docs/design/daemon-cli-split.md`, merged) left the shared substrate with three growth pains worth correcting before more is built on top.

- **`omnifs-mount-schema` is two domains in one crate.** It carries mount configuration (`Spec`, `Resolved`, `Catalog`, the user `auth`/`capabilities` blocks) and the provider contract (`ProviderManifest`, the WASM custom-section codec, route resolution, capabilities, config schema). They share a wall but not an audience.
- **Several type families are modelled more than once.** Auth appears as the user-config `Auth`, the provider-authored `ManifestAuthScheme`/`ManifestOAuthFlow`, the runtime `AuthScheme`/`OAuthFlow`/`AuthManifest`, and the store-side `CredentialKind`, bridged by a transform that exists only because two of those live in the same crate for the same consumers. `Spec` and `Resolved` are field-for-field twins differing only in `provider_id: Option<String>` vs `String`.
- **The on-disk layout is resolved in two places that have already drifted.** `crates/omnifs-cli/src/paths.rs` and `crates/omnifs-daemon/src/paths.rs` both derive an omnifs root from environment and fall back to `~/.omnifs`. After the layout was flattened on the CLI side (`credentials.json` and `providers/` at the root, no `data/` tier), the daemon copy still resolved the provider directory under the old nested layout. That is exactly the drift a shared definition prevents.

This document records the target crate structure. The governing principle: **a crate earns its existence by being shared across consumers or compiled to a different target (`wasm32-wasip2` provider builds vs host binaries). One-consumer logic stays a module, not a crate.** That is why `omnifs-api` (2 consumers) is healthy at its size, why the duplicated layout logic deserves a crate (2 consumers, already drifting), and why the CLI's `config.toml` handling does not (1 consumer).

## Dependency map

```
                                            -- depends-on points downward --

  omnifs-cli (omnifs)                                                              omnifs-daemon (omnifsd)
      | home . api . mount . provider . creds . auth                                   | home . api . mount . provider . host . fuse
      | (no wasmtime/fuser, no host)                                                   |
      |                                                                    +-----------+-----------+
      |                                                              omnifs-host                omnifs-fuse
      |                                                              core.provider.mount.         core . host
      |                                                              creds.auth.cache.wit.view
      |                                                                    |
      +--------------+--------------+--------------+--------------+--------+------+----------------------+
  omnifs-mount   omnifs-provider  omnifs-auth     omnifs-creds   omnifs-api    omnifs-cache       omnifs-sdk (+macros)
   core.provider     core         provider.creds     core       (serde.utoipa)    core              core.provider.wit
      |                 |            |                 |                              |                   |
      |                 |            |                 |                              |            providers (github, linear, db,
      |                 |            |                 |                              |            docker, dns, arxiv, ...) -> wasm
      v                 v            v                 v                              v                   v
  +----------------------------------------------------------------------------------------------------------------------+
  |  omnifs-core   (mime . postcard . serde . thiserror -- wasm-safe)                                                     |
  |     ^ fan-out: omnifs-sdk . providers(github,linear) . omnifs-cache . omnifs-fuse . omnifs-host . omnifs-creds .      |
  |                omnifs-mount . omnifs-cli . omnifs-itest   (everything wasm-safe + host sits here)                     |
  +----------------------------------------------------------------------------------------------------------------------+

  omnifs-home  <-- only -- omnifs-cli, omnifs-daemon          omnifs-wit  <--  omnifs-sdk, omnifs-host
  (std env + PathOverrides + under_root; host-only leaf)      (WIT bindings leaf)
```

`omnifs-core` is the wide wasm-safe leaf under almost everything; `omnifs-home` is a deliberately narrow host-only leaf with exactly two consumers. They are separate so the layout / `std::env` logic never reaches the wasm providers that sit on `core`. `omnifs_host::Dirs` is not a third layout authority: it is a bundle of already-resolved runtime dirs (cache + providers) that the daemon builds from its resolved `Paths` and hands to the `Runtime`, so the host is a path-receiver, not a resolver.

## Crates and responsibilities

| Crate | Change | Owns (responsibility) | Consumers |
|---|---|---|---|
| `omnifs-home` | NEW | The omnifs root and its on-disk shape: `Paths`, `PathOverrides`, `under_root`, the layout constants (`config.toml`/`credentials.json`/`mounts`/`providers`/`cache`), and `OMNIFS_HOME` + explicit override precedence. Host-only, the single layout authority. | CLI, daemon |
| `omnifs-provider` | SPLIT (from mount-schema) | The provider's self-description / contract: `ProviderManifest`, the WASM custom-section codec (`sections`/`records`), route resolution, `ProviderCapabilities`, `ConfigSchema`, and one auth-scheme model (`AuthScheme`/`AuthFlow`/`AuthManifest`) replacing the manifest-vs-wire duplication. | SDK, providers (wasm), host, daemon, CLI |
| `omnifs-mount` | SPLIT (from mount-schema) | The mount itself: a single `Mount` type (`Spec`+`Resolved` collapsed to `{ spec, provider_id }`), `Catalog`, and `Spec -> Resolved` resolution against `omnifs-provider`. Plus the sparse user `Auth` config. | CLI, daemon, host |
| `omnifs-core` | CHANGED | Validated newtypes and protocol types: `Id`, `Name`, `Path`/`Segment`, `ContentType`, `CredentialId`/`SchemeId`/`AccountId`. wasm-safe. Move `view.rs` (host cache records) out to `omnifs-host`. | nearly everything |
| `omnifs-creds` | UNCHANGED role | Credential store: `CredentialEntry`, `CredentialStore` (file / keychain / memory). Its `CredentialKind` merges with the unified auth discriminant. | CLI, host, auth |
| `omnifs-auth` | CHANGED dep | OAuth protocol client: `OAuthClient`, `OAuthRequest`, device/loopback/manual flows. Depends on `omnifs-provider` for the auth-scheme types instead of mount-schema. | CLI, host |
| `omnifs-api` | UNCHANGED | Control-plane HTTP DTOs: `VersionInfo`, `ReadyInfo`, `DaemonStatus`, `FrontendInfo`, `MountInfo`. The CLI/daemon wire contract. | CLI, daemon |
| `omnifs-daemon` (`omnifsd`) | CHANGED | Runtime daemon: control API, registry, FUSE frontend lifecycle. Deletes its `paths.rs` copy, uses `omnifs-home`. | binary |
| `omnifs-cli` (`omnifs`) | CHANGED | The CLI, plus a `Workspace` module (`config.toml` parse/author + the single mount-enumeration funnel) built on `omnifs-home`. Drops duplicated path logic. | binary |
| `omnifs-host`, `omnifs-fuse`, `omnifs-cache`, `omnifs-sdk` (+macros), `omnifs-wit`, `omnifs-inspector` | UNCHANGED | Runtime execution, FUSE, caches, provider authoring surface, WIT bindings, inspector. Not touched beyond `omnifs-host` receiving `view.rs`. | -- |

## Direct dependency edges

`omnifs-*` edges only (the precise version of the diagram):

| Crate | depends on |
|---|---|
| `omnifs-core` | -- (leaf) |
| `omnifs-home` | core (for `MountName`); std only otherwise |
| `omnifs-wit` | -- (leaf) |
| `omnifs-provider` | core |
| `omnifs-mount` | core, provider |
| `omnifs-creds` | core |
| `omnifs-auth` | provider, creds |
| `omnifs-api` | -- (serde, utoipa) |
| `omnifs-cache` | core |
| `omnifs-sdk` (+macros) | core, provider, wit |
| providers | sdk, provider, core |
| `omnifs-fuse` | core, host |
| `omnifs-host` | core, provider, mount, creds, auth, cache, wit |
| `omnifs-daemon` | home, api, mount, provider, host, fuse |
| `omnifs-cli` | home, api, mount, provider, creds, auth |

## omnifs-home (new)

The crate owns the layout names and the directory resolution that the CLI and daemon currently duplicate. Its public surface, lifted from the flattened `cli/src/paths.rs`:

```rust
// The single source of truth for layout names. Every concrete path (host
// default resolution and the in-container guest layout) is `root` joined
// with one of these; host and guest share the same flat shape.
const CONFIG_FILE: &str = "config.toml";
const CREDENTIALS_FILE: &str = "credentials.json";
const MOUNTS_SUBDIR: &str = "mounts";
const PROVIDERS_SUBDIR: &str = "providers";
const CACHE_SUBDIR: &str = "cache";
// default root: $OMNIFS_HOME, else $HOME/.omnifs

pub struct PathOverrides {        // explicit (CLI flag) relocations
    pub config_dir: Option<PathBuf>,
    pub cache_dir: Option<PathBuf>,
}

pub struct Paths {                // the fully resolved layout
    pub config_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub mounts_dir: PathBuf,
    pub providers_dir: PathBuf,
    pub credentials_file: PathBuf,
    pub config_file: PathBuf,
}

impl Paths {
    // Resolve from overrides, then OMNIFS_HOME, then the user's default home.
    pub fn resolve(overrides: PathOverrides) -> Self;

    // The one place that maps the omnifs structure to concrete paths. Host
    // default resolution and the in-container guest layout both build on this.
    pub fn under_root(root: &Path) -> Self;

    pub fn display(path: &Path) -> String;            // ~/.omnifs relativize
    pub fn mount_config_path(&self, name: &MountName) -> PathBuf;
    pub fn provider_path(&self, provider: &str) -> PathBuf;
}
```

Dependencies: `omnifs-core` (for `MountName`) and `std`. Nothing host-runtime, nothing wasm.

What stays in the CLI (does **not** move into `omnifs-home`): the two-pass `resolve_with_config`, because it locates and reads `config.toml`. `config.toml` is the host user's authoring surface, owned by the CLI `Workspace`; the daemon never reads it. `omnifs-home` provides `resolve(overrides)` and `under_root`.

The daemon side collapses cleanly: `omnifs-daemon/src/paths.rs` is deleted and replaced by `omnifs_home::Paths::resolve`, reading the `config_dir`, `cache_dir`, and canonical `providers_dir` fields it needs. `omnifs_host::Dirs` is then built from that `Paths` via one `From`, so the runtime's dir bundle never drifts from the resolved layout.

## Workspace (not a crate)

`Workspace` stays a module in `omnifs-cli`. It owns `config.toml` (immutable read view plus doc-preserving surgical mutators that cannot drop existing comments or unrelated sections) and the single mount-enumeration funnel (`config.toml` `[[mounts]]` merged with any per-file specs, the one path every command uses to list mounts). It has one consumer, the host CLI, so it is a module; it depends on `omnifs-home` for directory resolution and on `omnifs-mount` for `Spec`. Provider resolution (`Spec -> Resolved`) stays in `omnifs-mount`'s `Catalog`; credential materialization stays in the CLI `Session`.

## Sequencing

Each step is independently shippable. None requires the next.

**Tier A** (low risk, mostly intra-crate):

1. `omnifs-home` (new) + delete the daemon/CLI `paths.rs` duplication. The flattened `under_root` already exists on the `fix/auth-status-banner` branch and is the seed.
2. Collapse the auth-type families into one scheme model.
3. Collapse `Resolved` to `{ spec, provider_id }`.
4. The `Workspace` module in the CLI.

**Tier B** (the bigger re-cut, after A so less code moves):

5. Split `omnifs-mount-schema` into `omnifs-provider` and `omnifs-mount`.

Net crate-count change: `mount-schema` (1) becomes `omnifs-provider` + `omnifs-mount` (2), plus `omnifs-home` (1), for +2 crates. Each is cohesive with real multi-consumer sharing, while the *type* count drops as the auth families and the `Spec`/`Resolved` twins collapse.

> Naming note: `omnifs-home` was chosen over `omnifs-layout`, `omnifs-dirs`, and `omnifs-paths`; it reads as "the omnifs home and what lives under it" and ties to the `OMNIFS_HOME` env var. It is deliberately **not** `omnifs-config`: `config.toml` management lives in the CLI `Workspace`, not here, so "config" would point at the wrong thing.
