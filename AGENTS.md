# AGENTS.md

Operational and code-level rules for working in the omnifs repo. This file
is the index; the actual rules live in `.rules/*.md`. `CLAUDE.md` is a
symlink to this file.

## Start here

1. **Read `docs/repo-intent.md` once.** Project mission, hard architectural
   commitments, planned directions, what "good" looks like.
2. Skim this index. The trigger lists below tell you which `.rules/*.md`
   files to open for the task in front of you, and which to update as part
   of the change you're making.
3. Open only the files whose triggers fire. You should not need to read
   every rules file at the start of a session.

## Rules index

For each file, **Read** triggers tell you when to open it; **Update**
triggers tell you when your change requires editing it. If you're making a
change that fires an Update trigger but you didn't read the file first,
read it now.

### [`.rules/workflow.md`](./.rules/workflow.md)

- **Read when:** starting a session; building; running tests; running the
  project locally; a build/test command isn't doing what you expect.
- **Update when:** adding or changing a build target, a `just` recipe, the
  Docker Compose flow, the provider build pipeline (e.g. reintroducing a
  preview1 adapter step), test-harness conventions, or interactive shell
  defaults baked into the image.

### [`.rules/auth.md`](./.rules/auth.md)

- **Read when:** changing auth flow, credential injection, secret handling,
  or git remote/clone behavior; before suggesting an SSH ↔ HTTPS transport
  change; touching anything that consumes `GITHUB_TOKEN` or
  `SSH_AUTH_SOCK`.
- **Update when:** adding a new credential source; changing how tokens
  reach the host or providers; switching git clone transport; adding a new
  auth-related provider capability; changing the operational contract for
  required host setup.

### [`.rules/debugging.md`](./.rules/debugging.md)

- **Read when:** something is failing at runtime — `Input/output error` on
  a mount path, hangs on `ls`/`cd`, silent clone failures, wrong FUSE
  results. Read before forming a theory; user-visible probes beat
  speculation.
- **Update when:** discovering a new failure mode worth triaging; a new log
  surface or trace channel appears; `omnifs status` grows new fields; the
  "expected noise" set changes.

### [`.rules/caching.md`](./.rules/caching.md)

- **Read when:** touching the host browse cache, FUSE notifier, or
  invalidation logic; a provider is tempted to memoize; before adding any
  "freshness" or "TTL" knob.
- **Update when:** changing tier sizing or thresholds; adding or removing a
  cache surface; changing invalidation semantics (`event-outcome`, FUSE
  notifier); adding a new preload or sibling-files mechanism; modifying
  how cache effects fold into terminals.

### [`.rules/provider-sdk.md`](./.rules/provider-sdk.md)

- **Read when:** authoring or modifying a provider; touching `omnifs-sdk` /
  `omnifs-sdk-macros`; changing the WIT; working on host-side dispatch; a
  path isn't resolving the way you expect.
- **Update when:** adding, renaming, or removing a handler attribute or
  top-level macro; changing the WIT contract; changing the browse-method
  surface (`lookup_child` / `list_children` / `read_file` / streaming);
  changing dispatch precedence, auto-navigability, or exhaustiveness rules
  (also update `docs/design/path-dispatch-and-listing.md`); changing the
  callout/resume protocol shape.

### [`.rules/gotchas.md`](./.rules/gotchas.md)

- **Read when:** writing or reviewing provider code; touching the FUSE
  layer; sizing/streaming a projected file; working with hashmaps inside a
  provider; before writing your first new provider handler.
- **Update when:** discovering a new code-level surprise that an agent
  could plausibly regress without a written-down warning. Append rather
  than rewrite — the framing of a footgun usually matters.

### [`.rules/code-style.md`](./.rules/code-style.md)

- **Read when:** refactoring; introducing an abstraction; changing a public
  contract (WIT, SDK macros, host browse surface); renaming something
  user-visible; merging multi-phase orchestration into a hot path;
  reviewing a PR for fit.
- **Update when:** an architectural commitment changes; a new
  design-judgment heuristic earns its place from real PR experience; the
  design-status convention changes; a new contract guardrail is added.

## Designs of record

- **Project intent:** [`docs/repo-intent.md`](./docs/repo-intent.md)
- **Design index:** [`docs/design/README.md`](./docs/design/README.md) —
  aggregate status table for every design doc.
- Status convention is documented in
  [`.rules/code-style.md`](./.rules/code-style.md).

## When to add a new rules file

Add `.rules/<topic>.md` only when:

- Three or more rules accumulate in one domain that doesn't fit any
  existing file, **and**
- An agent benefits from reading those rules proactively (not just
  searching when they hit a problem).

Add a section above with both `Read when` and `Update when` triggers. Keep
the rules file itself focused on the rules; triggers live here in the
index.

## Provider-specific rules

Top-level `.rules/*.md` covers cross-cutting concerns: SDK contracts, host
behavior, code-wide style. **Rules specific to one provider live with that
provider**, not here.

- Single provider, a handful of rules → `providers/<name>/AGENTS.md`.
- Larger surface that benefits from topic split →
  `providers/<name>/.rules/*.md` indexed by `providers/<name>/AGENTS.md`,
  same pattern as this file.

Examples that belong in a provider's local file, not the top-level
`.rules/`: GitHub rate-limit / ETag handling, arXiv pagination quirks,
DNS resolver fallback policy, the hybrid issue/PR pagination strategy,
repo-clone cache-key conventions.

If you find yourself reaching for `.rules/provider-sdk.md` to write a
rule that only applies to one provider, redirect to that provider's own
file.
