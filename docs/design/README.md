# Design docs

Aggregate index of every design doc in this repo, with current status.

For the convention behind the `Status:` field on each doc, see
`.rules/code-style.md` ("Design status convention").

For the project's high-level intent and architectural commitments, see
`docs/repo-intent.md`.

## Index

| Doc                                                | Status                                | Scope                                                                  |
|----------------------------------------------------|---------------------------------------|------------------------------------------------------------------------|
| [docs/design/path-dispatch-and-listing.md](./path-dispatch-and-listing.md) | accepted                              | route registration, lookup/list dispatch, FUSE cache, listing semantics |
| [docs/design/projected-file-sizes.md](./projected-file-sizes.md)           | implemented on `design/projected-file-sizes` branch | WIT, host FUSE + cache schema, SDK projection API, providers          |
| [design/protocol-shape.md](../../design/protocol-shape.md)                 | draft, ready to implement             | WIT, host runtime, SDK + macros, all providers, tests                  |
| [design/protocol-shape-handoff.md](../../design/protocol-shape-handoff.md) | implementation prompt (working material, not a design) | implementation guide for `protocol-shape.md`                            |
| [design/mutations-via-git.md](../../design/mutations-via-git.md)           | proposed                              | mutation model: mounted scope as Git repo, `git-remote-omnifs` reconcile |
| [docs/future/async-http.md](../future/async-http.md)                       | future / north star                   | direct `wasi:http` redesign, gated on async-component readiness        |

## Conventions reminder

- The doc's own `Status:` line is the source of truth. Update both the doc
  and this table when status changes.
- Allowed primary states: `proposed`, `accepted`, `implemented on <branch>`,
  `superseded by <path>`, `historical`. `future` / `north star` is reserved
  for `docs/future/` material that explicitly depends on external readiness.
- Working materials (implementation prompts, scratch notes) are not
  designs. Mark them as such inline.
- Two physical directories exist today: `docs/design/` (mostly accepted /
  implemented) and `design/` (mostly draft / proposed). The split is
  historical, not load-bearing; consolidating is a future cleanup.
