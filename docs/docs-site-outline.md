# omnifs docs site — outline

Status: proposed
Scope: information architecture for the public documentation site (not the
internal `docs/design/` RFCs, which this outline reuses as source material)

This is the recommended structure for omnifs' user-facing docs site. It is
organized [Diátaxis](https://diataxis.fr)-style: tutorials (learning),
how-to guides (tasks), reference (lookup), and explanation (understanding),
plus contributor/maintainer tracks kept out of the main reader path.

## Audiences

The site serves four readers. Every page should know which one it is for.

1. **Users** — humans who want to `cd`/`ls`/`cat`/`grep` the world. Want install,
   quickstart, provider reference, troubleshooting.
2. **Agents** — people wiring omnifs into AI agents/tools. Want the "paths as the
   universal API" model and stable path contracts.
3. **Provider authors** — people building a new WASM provider with the SDK. Want
   the authoring guide, handler attributes, and the WIT/protocol reference.
4. **Contributors & maintainers** — people hacking on the host/CLI or cutting
   releases. Want architecture, dev workflow, and release process.

## Tooling note

No docs-site framework exists in the repo yet. Recommended:
[VitePress](https://vitepress.dev) or [Astro Starlight](https://starlight.astro.build)
— fast, low-config, good for a Rust/WASM project that already ships an npm
package. The nav tree below maps directly to a sidebar config. Keep authored
prose under `docs/` and treat `docs/design/` + `docs/future/` as linked RFCs.

---

## Top-level navigation

```
Home
1. Introduction
2. Getting started
3. Guides
4. Providers
5. CLI reference
6. Concepts (how it works)
7. Building providers (SDK)
8. Contributing
9. Releasing & distribution
10. Reference & appendix
```

---

## 0. Home / landing

- Tagline: *the universe, mounted on your filesystem.*
- The pitch: Plan 9 was right; APIs → paths, for humans and agents alike.
- 30-second demo (asciinema/video), the 3-layer diagram, alpha warning.
- Three primary CTAs: **Install**, **Browse providers**, **For agents**.
- Source: `README.md` (top section + diagram).

## 1. Introduction

Explanation-first, no commands. Gets a newcomer to "I get it" fast.

- **What is omnifs** — projected filesystem mirroring external services into local
  paths via FUSE. Source: `README.md`, `CLAUDE.md` (project model).
- **Why omnifs** — everything-is-a-file thesis; the cost of APIs/SDKs/pagination;
  one read interface for humans and agents. Source: `README.md` ("For agents").
- **How it works (at a glance)** — shell ⇄ FUSE host ⇄ WASM providers ⇄ services;
  callout runtime; git-backed write-back (WIP). Source: `README.md` "How it works".
- **Project status** — alpha; Linux-only mount, CLI on macOS/Linux via a Linux
  container; macOS/Windows planned. Source: `README.md`, `CLAUDE.md`.

## 2. Getting started

Tutorial track. One happy path, end to end, no forks.

- **Prerequisites** — Node/npm, Docker (Desktop on macOS), SSH agent with a GitHub
  key for `/github` repo clones. Source: `README.md` Prerequisites.
- **Install** — `npm install -g @0xff-ai/omnifs`; npm installs only the native
  CLI, the runtime image is pulled on `omnifs up`. Source: `README.md`,
  `docs/design/npm-distribution.md`.
- **Quickstart** — `omnifs init github` → `omnifs up` → `omnifs shell`; first
  `cd /github/...`. Source: `README.md` Quickstart.
- **Guided onboarding** — `omnifs setup` walkthrough (detect OS, explain Docker,
  pick providers, launch). Source: `crates/cli` setup command.
- **Platform notes** — Linux native vs macOS via Docker Desktop Linux container
  (mount is inside the container, reached through `omnifs shell`, not Finder).
- **Next steps** — links into Guides and Providers.

## 3. Guides (how-to, task-oriented)

Each is a short "to do X, run Y" recipe. Independent, skimmable.

- **Browse the filesystem** — `cd`/`ls`/`cat`/`grep`/`find`; what behaves like a
  real file (the bash-tool compatibility contract). Source: `CLAUDE.md` design
  invariants.
- **Work with GitHub** — repos, issues, PRs, diffs, actions/CI; clone-on-list.
  Source: `README.md` GitHub table.
- **Query DNS** — records, `@resolver` selection, reverse lookups. Source:
  `README.md` DNS table.
- **Browse arXiv** — papers by id/category/author/search, PDFs, source, versions.
  Source: `README.md` arXiv table.
- **Use omnifs with AI agents** — paths as a tool surface; stable path contracts;
  read-the-file-instead-of-the-API pattern. Source: `README.md` "For agents".
- **Authenticate providers** — OAuth login (`omnifs auth login`), device vs PKCE
  flows, importing existing tokens (`omnifs auth import`), status. Source:
  `docs/oauth.md`, `docs/design/host-auth.md`, `crates/cli` auth.
- **Manage mounts** — `omnifs init`, `omnifs mounts`, `omnifs reset`; where configs
  live (`~/.omnifs/config/mounts`). Source: `docs/design/mount-lifecycle.md`.
- **Container lifecycle** — `omnifs up` / `down` / `shell` / `logs [-f]`.
- **Inspect & debug** — `omnifs status`, `omnifs doctor`, `omnifs logs`,
  `omnifs inspect` (FUSE/provider/callout event stream). Source: `crates/cli`,
  `docs/design/inspector-emission-architecture.md`, `CLAUDE.md` runtime debugging.
- **Troubleshooting** — SSH agent issues, `Input/output error` on repo paths,
  mount-missing checks. Source: `README.md` SSH troubleshooting, `CLAUDE.md`.

## 4. Providers (reference)

One page per shipped provider; a path table + auth + notes each. Plus the
roadmap. Mirror the README tables but go deeper.

- **Provider catalog** — what a provider is, the mount table, how to add/enable.
- **GitHub** (`/github`) — full path table, SSH clone model, auth (device OAuth,
  read-only no-scope tokens). Source: `README.md`, `docs/oauth.md`.
- **DNS** (`/dns`) — record paths, resolvers, reverse. Source: `README.md`.
- **arXiv** (`/arxiv`) — per-paper subtree, scopes, versions. Source: `README.md`,
  `docs/design/arxiv-recent-submissions.md`.
- **Database** (`/db` or mount) — SQLite, read-only browse; preopened paths.
  Source: `docs/design/providers/db.md`, `providers/db`.
- **Docker** — container/image projection. Source: `providers/docker`.
- **Linear** (`/linear`) — teams/issues via GraphQL, PKCE OAuth `read` scope.
  Source: `docs/design/providers/linear.md`, `providers/linear`.
- **Provider roadmap** — Hugging Face, S3, OCI, Kubernetes, Postgres, Slack/
  Discord, and per-provider expansions. Source: `README.md` roadmap table.

## 5. CLI reference

Generated/structured per-command. One row/page per subcommand with synopsis,
flags, examples. Source of truth: `crates/cli/src/cli.rs` doc comments.

- **Global** — `-v/-vv`, `RUST_LOG`, `--image`/`OMNIFS_IMAGE`.
- **Lifecycle** — `up`, `down`, `shell`, `logs`, `status`, `inspect`.
- **Onboarding & config** — `setup`, `init`, `mounts`, `reset`.
- **Auth** — `auth login` / `logout` / `import` / `status`.
- **Diagnostics** — `doctor`, `version` (`--detail`), `completions`.
- **Contributor** — `dev` (source-checkout sandbox).
- **Internal/hidden** — `daemon`, `debug` (documented as internal, not for users).

## 6. Concepts (how it works — explanation)

The deep "why it's built this way" track. Each page can link to the matching
`docs/design/*` RFC for full detail.

- **Architecture overview** — host, providers, runtime, the three layers.
- **The single path space** — absolute protocol paths; one namespace. Source:
  `docs/design/protocol-paths.md`.
- **Provider model** — WASM components, the `omnifs:provider` WIT interface,
  free-function handlers. Source: `docs/design/protocol-provider-model.md`,
  `CLAUDE.md` provider architecture.
- **Callout runtime** — providers describe needs, host executes; strict request/
  response; effects in terminals. Source: `docs/design/protocol-shape.md`.
- **Path dispatch & listing** — routing precedence, auto-navigable dirs, listing
  exhaustiveness, lookup vs readdir authority. Source:
  `docs/design/path-dispatch-and-listing.md`.
- **Caching model** — host-owned, capacity-bounded, no TTLs, invalidation signals,
  preload/sibling files. Source: `docs/design/cache-architecture.md`.
- **File attributes** — Size/Bytes/ReadMode/Stability/version evidence; honest
  stat, direct_io. Source: `docs/design/file-attributes.md`,
  `docs/design/projected-file-sizes.md`.
- **Auth & credentials** — auth manifest, credential store (keychain/file),
  `CredentialKey` wire form. Source: `docs/design/host-auth.md`.
- **Cloning** — SSH transport, forwarded agent, clone manager, treerefs/bind
  mounts. Source: `CLAUDE.md` cloning, `README.md`.
- **WASM sandbox substrate** — Wasmtime/WASI plumbing, embedded tools. Source:
  `docs/design/wasm-sandbox-substrate.md`.
- **Mount lifecycle & effective config** — load → effective config → credential
  materialization → container. Source: `docs/design/mount-lifecycle.md`.

## 7. Building providers (SDK guide)

The "write your own provider" track — the highest-leverage docs for growth.

- **Overview** — what a provider is, anatomy of a provider crate, the
  `wasm32-wasip2` target. Source: `CLAUDE.md`, `crates/omnifs-sdk`.
- **Project setup** — workspace member, `wit_bindgen::generate!`,
  `#[provider(mounts(...))]`. Source: `crates/omnifs-sdk-macros`, `providers/*`.
- **Handlers** — `#[dir]`, `#[file]`, `#[treeref]`, `#[bind]`, `#[mutate]`; auto-
  navigable prefixes; per-segment validators. Source: `CLAUDE.md`,
  `docs/design/path-dispatch-and-listing.md`.
- **Typed subtrees** — `#[subtree] impl` blocks and prefix-capture dispatch.
- **Config** — `#[config]`, JSON instance config, `initialize()`.
- **Projections & file attributes** — declaring Size/Bytes/ReadMode/Stability via
  the `Projection` API. Source: `docs/design/file-attributes.md`.
- **Project everything you fetched** — sibling files (`with_sibling_files`),
  `Projection::preload`/`preload_many`. Source: `CLAUDE.md` caching model.
- **Auth manifest** — `omnifs.provider.json`, `static-token`/`oauth`, embedded
  `omnifs.provider-metadata.v1`. Source: `docs/oauth.md`, `docs/design/host-auth.md`.
- **Callouts** — HTTP fetch, git open, archive; `resume()`; continuations.
  Source: `docs/design/protocol-shape.md`, `crates/omnifs-tool-archive`.
- **Cache invalidation** — `on-event`, `invalidate-paths`/`-prefixes`.
- **Testing providers** — `--target wasm32-wasip2 --no-run`, host-target
  `#[cfg(test)]`, the smoke harness, bash-tool compatibility. Source: `CLAUDE.md`.
- **WIT reference** — `wit/provider.wit` browse surface (`lookup_child`,
  `list_children`, `read_file`), result/terminal variants. Source: `wit/`.

## 8. Contributing

Contributor track. Keep separate from user docs.

- **Repo layout & crates** — `cli`, `host`, `inspector`, `omnifs-auth`,
  `omnifs-creds`, `omnifs-model`, `omnifs-mount-schema`, `omnifs-sdk`,
  `omnifs-sdk-macros`, `omnifs-tool-archive`, `providers/*`. Source: `Cargo.toml`,
  `CLAUDE.md`.
- **Dev workflow** — `omnifs dev` (build image, materialize secrets/fixtures,
  launch). Source: `CLAUDE.md` supported workflow, `crates/cli/.../dev.rs`.
- **Build & validation** — `cargo fmt`, `cargo nextest run`, `just check`,
  `just providers-check/-build`. Source: `CLAUDE.md`, `justfile`.
- **Testing & smoke harness** — `tests/smoke/`, runtime validation via the live
  container, traversal checks. Source: `CLAUDE.md`.
- **Observability / inspector** — event stream architecture for debugging.
  Source: `docs/design/inspector-emission-architecture.md`.
- **Coding conventions** — small/local changes, `From`/`TryFrom` at boundaries,
  `hashbrown` maps, test-quality bar, design judgment. Source: `CLAUDE.md`.
- **Design docs (RFC index)** — link table of `docs/design/*` and `docs/future/*`
  with status. Source: this repo.

## 9. Releasing & distribution (maintainer)

- **Release process** — `just release-cut`, PR → merge → `release.yml` via
  `workflow_run`. Source: `RELEASING.md`, `CLAUDE.md`.
- **Version coupling** — npm / Cargo / image tag share one semver; prerelease
  rules. Source: `CLAUDE.md`.
- **npm packaging** — `npm/platforms.json` as source of truth, bin shim, resolve
  helper. Source: `docs/design/npm-distribution.md`, `CLAUDE.md`.
- **Runtime image** — GHCR, `build-runtime-image.sh`, tag promotion.
- **Native CI pipeline** — cargo-zigbuild, Docker only for image assembly.
  Source: `CI-NATIVE.md`.

## 10. Reference & appendix

- **Mount config schema** — JSON shape, `static-token`/`oauth`, `token_env`/
  `token_file`. Source: `crates/omnifs-mount-schema`, `CLAUDE.md`.
- **Glossary** — provider, mount, callout, treeref, subtree, projection, effective
  config, inode table, router.
- **Roadmap / what's coming** — core + provider roadmap. Source: `README.md`.
- **FAQ** — macOS mount visibility, read-only tokens, why a container, why SSH.
- **Future / RFCs** — async HTTP (`docs/future/async-http.md`), mutations via git
  (`docs/future/mutations-via-git.md`).
- **Changelog & license** — `CHANGELOG.md`, MIT OR Apache-2.0.

---

## Source-material coverage map

Every existing doc has a home in the site, so nothing is orphaned:

| Existing source | Lands in |
| --- | --- |
| `README.md` | Home, Introduction, Getting started, Providers |
| `docs/oauth.md` | Guides → Authenticate; Providers (GitHub/Linear); SDK auth manifest |
| `docs/design/host-auth.md` | Concepts → Auth & credentials |
| `docs/design/mount-lifecycle.md` | Guides → Manage mounts; Concepts → Mount lifecycle |
| `docs/design/path-dispatch-and-listing.md` | Concepts → Path dispatch; SDK → Handlers |
| `docs/design/cache-architecture.md` | Concepts → Caching |
| `docs/design/file-attributes.md`, `projected-file-sizes.md` | Concepts → File attributes; SDK → Projections |
| `docs/design/protocol-*.md` | Concepts → Provider model / Callout runtime; SDK → WIT reference |
| `docs/design/wasm-sandbox-substrate.md` | Concepts → WASM sandbox |
| `docs/design/inspector-emission-architecture.md` | Guides → Inspect; Contributing → Observability |
| `docs/design/cli-redesign.md` | CLI reference |
| `docs/design/npm-distribution.md` | Getting started → Install; Releasing → npm |
| `docs/design/arxiv-recent-submissions.md` | Providers → arXiv |
| `docs/design/providers/db.md`, `linear.md` | Providers → Database / Linear |
| `docs/future/*.md` | Reference → Future / RFCs |
| `CI-NATIVE.md` | Releasing → Native CI |
| `RELEASING.md` | Releasing → Release process |
| `CLAUDE.md` / `AGENTS.md` | Contributing (conventions), Concepts, SDK guide |
| `crates/cli/src/cli.rs` | CLI reference (source of truth) |

## Build sequencing (recommended)

1. **MVP (user-facing):** Home, Introduction, Getting started, Providers (GitHub/
   DNS/arXiv), CLI reference, Troubleshooting. This unblocks adoption.
2. **Concepts + SDK guide:** enables external provider authors — the growth lever.
3. **Contributing + Releasing:** can stay close to `CLAUDE.md`/`RELEASING.md`
   until external contributors arrive.
