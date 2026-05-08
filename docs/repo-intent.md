# omnifs — repo intent

This document captures the "why" behind omnifs, the design ethos, and the
non-obvious decisions that shape the codebase. It exists so that any agent (or
new contributor) reading the code can answer "is the change I'm about to make
aligned with what this project is trying to be?" before changing things.

This is a companion to:

- `README.md` — user-facing pitch, quickstart, and provider catalog.
- `AGENTS.md` — operational guidance for working in the repo (workflow, auth,
  build/test).
- `CLAUDE.md` — codebase-level rules and gotchas for AI coding assistants.
- `docs/design/*.md` — accepted design notes; load-bearing for dispatch and
  projected sizing.
- `design/*.md` — proposed/in-flight protocol design notes.
- `docs/future/*.md` — north-star redesigns gated on external readiness.

If something below contradicts an accepted design doc, the design doc wins —
flag the conflict and update this page.

---

## What omnifs is, in one sentence

omnifs is a **projected filesystem** that turns external services (GitHub,
arXiv, DNS, eventually Linear / S3 / Kubernetes / …) into paths you can `cd`,
`ls`, `cat`, `grep`, and — eventually — `git commit`/`git push` to mutate.

The name is from Latin *omni-* ("all"): the universal filesystem. It is a
deliberate revival of the Plan 9 idea ("everything is a file") for a world
that has spent twenty years moving in the opposite direction (every service
is a bespoke API + SDK).

## Why it exists

Two audiences, one substrate, **co-equal by design**.

1. **Humans.** Reading a remote service should feel like reading a directory.
   Browsing issues, fetching a paper PDF, or checking a DNS record should not
   require an SDK, an OAuth flow, or a separate UI tab. `cat`, `ls`, `grep`,
   `find`, `rg`, `du`, `diff` are universal and good enough for most read
   paths.
2. **Agents.** LLM-driven agents already know how to use a filesystem. They
   reliably struggle with ad hoc HTTP APIs, pagination, auth flows, and rate
   limits. omnifs reframes "tool use" as "open a path and read it". The
   filesystem becomes the universal API.

These audiences share the same backend by design. There is no separate
"agent mode". The same FUSE projection serves a human shell session and an
agent's `read_file` tool call. When the two pull in different directions
(e.g. terse names vs. self-describing paths), neither audience automatically
wins; the tradeoff is a third axis to be designed against, not collapsed.

## The core idea, in three layers

```
shell / agent  ──FUSE──▶  omnifs host  ──effects/callouts──▶  WASM provider  ──HTTP/git──▶  remote service
```

1. **A FUSE filesystem** mounted at `/omnifs` (with provider mounts symlinked
   from `/<mount>` for ergonomics). The host owns inode allocation, caching,
   readdir/lookup semantics, and FUSE invalidation. This layer is in
   `crates/host/src/fuse/` and `crates/host/src/runtime/`.

2. **Sandboxed WASM providers** (`wasm32-wasip2` components) that implement
   the `omnifs:provider` WIT interface (`wit/provider.wit`). Each provider
   projects one external domain into one mount point. Providers are pure data
   shapers; they cannot touch the network or the disk directly.

3. **A request/response callout protocol.** Providers declare what they need
   ("fetch this URL", "open this git repo"); the host executes the work and
   resumes the provider with the result. This is the load-bearing reason the
   architecture is the way it is — see "Why callouts" below.

### Why WASM components specifically

All three of these reasons matter, and removing any one of them weakens the
case for the others:

- **Sandboxing / capability enforcement.** Providers must not see tokens,
  open sockets, or read disk on their own. Capabilities (HTTP domains, git
  remotes, memory limits) are declared and host-enforced.
- **Plugin distribution.** A `.wasm` file is the same artifact on every host
  OS. Drop one in `~/.omnifs/plugins/` and it mounts. No per-OS native
  builds, no compiler toolchain at install time.
- **Ecosystem bet.** The WASM component model is the substrate this project
  expects to live on long-term. Investments in the SDK, the WIT, and the
  callout protocol assume that bet.

## Hard architectural commitments

These are non-negotiable in the current codebase. Don't relitigate them in a
casual PR; they exist for reasons that took multiple iterations to discover.

### Providers are sandboxed and effect-driven

A provider cannot make an HTTP request, open a socket, write to disk, hold a
TTL cache, run a background timer, or otherwise reach outside its sandbox.
Everything the provider needs is expressed as a `callout` that the host runs.
**This is a hard rejection criterion for any PR that tries to give providers
their own escape hatch.**

This is what enables:

- **Centralized auth and credential injection.** Tokens never enter provider
  memory.
- **Centralized capability enforcement.** The host checks declared HTTP
  domains, git remotes, memory limits before doing the work
  (`runtime/capability.rs`).
- **Centralized caching.** Providers can't drift because they cannot cache.
- **Concurrency on a single instance.** The host suspends/resumes providers
  on a `correlation-id`, so many in-flight requests share one component
  instance.
- **A future redesign path.** When async components mature, the callout
  protocol can disappear in favor of direct `wasi:http` (see
  `docs/future/async-http.md`). That's a distant north star — revisit
  periodically, but it is not driving current design.

### The host owns all caching

Two-tier (`L0` in-memory moka, `L2` redb-backed). **No TTLs.** Eviction is
by capacity or by explicit invalidation, either from `event-outcome` returned
by `on-event` handlers or via the FUSE notifier path. Providers must not
reintroduce their own LRUs or time-based expiration. See
`crates/host/src/cache/`.

### `lookup` is authoritative; `readdir` may be non-exhaustive

This is the procfs / 9P invariant, formalized in
`docs/design/path-dispatch-and-listing.md`. Listings carry an `exhaustive`
flag; the host only short-circuits negative lookups when it is sound to do
so. Read that doc before changing dispatch logic.

### The mounted scope IS the git repo (planned mutation model — next milestone)

Mutations are not implemented yet, but the design that wins is
`design/mutations-via-git.md`, and **landing it is the next big milestone**
on the roadmap once the read model is stable enough.

The mounted scope (e.g. `/github/foo/bar`) is itself a real git repository.
Users edit, `git add`, `git commit`, `git push`, and a custom
`git-remote-omnifs` helper translates the push into the provider's
`plan-mutations` / `execute` reconcile pipeline.

This is preferred over directly-writable projected files because it gives:

- Familiar UX (every developer already knows git).
- Atomicity at commit boundaries.
- Free audit trail and revertibility.
- Clean separation between "draft" (local) and "execute" (push).

Don't reintroduce write-on-fsync semantics for projected files.

### Linux first, Linux only (today)

macOS and Windows are listed as planned, but the codebase is Linux-only right
now. `compose.yaml` + Docker is the supported user workflow. Don't add
`diskutil` / macFUSE / WinFsp paths unless explicitly asked. See `AGENTS.md`.

### Path-first SDK, not effect-first

Providers are authored as **typed route handlers**, not as imperative tree
walkers. Attributes (`#[dir]`, `#[file]`, `#[treeref]`, `#[bind]`,
`#[mutate]`) declare the path family a free function answers; the host walks
the route table. This was a deliberate redesign — the previous effect-style
API is gone. Don't bring it back.

## Design ethos

Distilled from `AGENTS.md`'s "Design judgment" section and observable in the
codebase:

- **Prefer the simpler end-to-end flow over the purer local abstraction.**
  An abstraction that adds a hop in the common case loses to direct dispatch.
- **Single-phase over multi-phase on the hot path.** Fold cache effects into
  the terminal that produced them rather than emitting them as a second
  channel. (This is why `preload` lives on `dir-listing` / `lookup-entry` and
  invalidations live on `event-outcome` — see `design/protocol-shape.md`.)
- **Project everything you've already paid for.** If a provider has fetched a
  payload that contains data for sibling files or nested entries, return it
  in `sibling-files` / `preload`. Don't force the host into another round
  trip when the bytes are already in hand.
- **Reuse source-of-truth terms.** Don't invent host-internal names for
  public WIT/SDK surfaces. Semantic fit beats code reuse.
- **Remove transitional glue once the direct path lands.** Bridge layers are
  acceptable while migrating; once the direct path exists, delete the bridge
  rather than letting it harden into architecture.

## Goals beyond the read model

These are real planned directions, not just possibilities:

- **Mutations via git push** (`design/mutations-via-git.md`) — the next big
  milestone.
- **Open plugin marketplace.** Third-party providers are a first-class goal,
  not an afterthought. That makes signing, versioning, capability auditing,
  and a sane discovery story all in-scope eventually.
- **Real mirroring / offline snapshots.** Lazy projection is the current
  default, but the long-term plan includes replayable sync and offline
  snapshots so a tree can persist independent of connectivity. README's
  "mirror" wording is intentional, not just lazy-projection shorthand.

## Things we are explicitly not committing to (yet)

- **Path schema stability.** User-visible path layouts (e.g.
  `/github/{owner}/{repo}/_issues/{filter}/{n}/title`) are fluid throughout
  the alpha. Don't treat them as a versioned API surface yet.
- **A final auth model.** Today: env vars, mounted secret files, forwarded
  SSH agent. The end state is undecided — somewhere between an OS-keyring /
  credential-broker model and a capability-declared model where providers
  state their auth needs and the host brokers credentials. Either direction
  is plausible; the current shape is scaffolding.
- **A general-purpose VFS toolkit.** omnifs is opinionated about projection
  semantics, dispatch rules, and the callout protocol. It is not trying to be
  a "build any FUSE filesystem you want" framework.
- **A drop-in API gateway.** omnifs prefers to project domain-shaped
  *paths*, not to mirror an upstream's REST shape verbatim. The schema is
  designed for human/agent navigability, not API-faithfulness.
- **Streaming / live tailing as the primary mode.** The WIT reserves
  streaming arms but the live path is request/response. Long-lived watches
  are explicitly out-of-scope until proven necessary.
- **Atomicity across upstreams that don't support it.** The mutation design
  is explicit that omnifs will not invent transactions the underlying
  service doesn't have.

## What "good" looks like in this repo

- A new provider is mostly declarative: a few `impl` blocks with `#[dir]` /
  `#[file]` / `#[treeref]` attributes, a `Config` struct, an `initialize`,
  and free-function handlers that do `let body = cx.http().get(url).send()?;
  parse_model(&body)`.
- A new host feature does not require provider changes unless the protocol
  itself changed.
- Cache behavior is observable and controllable from the host — no per-provider
  cache state.
- Mutations, when added, route through git push and `plan-mutations` /
  `execute`, not through write syscalls on projected files.
