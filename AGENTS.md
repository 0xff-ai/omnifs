# AGENTS.md

Operational and code-level rules for working in the omnifs repo. This file
is the index; the actual rules live in `.rules/*.md`. `CLAUDE.md` is a
symlink to this file.

## Start here

1. **Read `docs/repo-intent.md` once.** Project mission, hard architectural
   commitments, planned directions, what "good" looks like.
2. Skim this index. Note the read-triggers below.
3. Open the specific `.rules/*.md` files whose triggers fire for the task
   you're starting.

You should not need to read every rules file at the start of a session.
Read on demand, guided by the triggers.

## Rules index

Each file declares its own `Read when:` (open it) and `Update when:` (edit
it as part of your change). The summary below is the at-a-glance trigger
list — open the file for the authoritative version.

| File                          | Read when…                                                                                                                                                  |
|-------------------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------|
| [`.rules/workflow.md`](./.rules/workflow.md)         | Starting a session; building; running tests; running the project locally; a build/test command isn't doing what you expect.                                  |
| [`.rules/auth.md`](./.rules/auth.md)                 | Changing auth flow, credential injection, secret handling, or git remote/clone behavior; before suggesting an SSH ↔ HTTPS transport change.                   |
| [`.rules/debugging.md`](./.rules/debugging.md)       | Something is failing at runtime — `Input/output error`, hangs on `ls` / `cd`, silent clone failures, wrong FUSE results. Read before forming a theory.        |
| [`.rules/caching.md`](./.rules/caching.md)           | Touching the host browse cache, FUSE notifier, invalidation logic, or a provider tempted to memoize. Before adding any "freshness" or "TTL" knob.            |
| [`.rules/provider-sdk.md`](./.rules/provider-sdk.md) | Authoring or modifying a provider; touching `omnifs-sdk` / `omnifs-sdk-macros`; changing the WIT; working on host-side dispatch; a path isn't resolving right. |
| [`.rules/gotchas.md`](./.rules/gotchas.md)           | Writing or reviewing provider code; touching the FUSE layer; sizing/streaming a projected file; before writing your first new provider handler.              |
| [`.rules/code-style.md`](./.rules/code-style.md)     | Refactoring; introducing an abstraction; changing a public contract (WIT, SDK macros, host browse surface); reviewing a PR for fit.                          |

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

Add a row to the index above with a clear `Read when` trigger. Include
both `Read when:` and `Update when:` headers in the new file.

When a rule no longer matches reality, update the rules file in the same
PR as the behavior change. Each rules file's `Update when:` header tells
you whether your change qualifies.
