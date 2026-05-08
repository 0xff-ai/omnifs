# Code style and contract guardrails

**Read when:** about to refactor, introduce an abstraction, change a public
contract (WIT, SDK macros, host browse surface), rename something
user-visible, or merge a multi-phase orchestration into a hot path. Read
before reviewing a PR for "does this fit the project's instincts."

**Update when:** an architectural commitment changes, a new design-judgment
heuristic earns its place from real PR experience, the design-status
convention changes, or a new contract guardrail is added.

---

## Codebase expectations

- Keep changes small and local.
- Prefer preserving the current architecture: inode table, router,
  providers, GitHub cache/scheduler/poller, clone manager.
- Do not silently change the auth model or transport model. If switching
  clone transport from SSH to HTTPS/token, call that out explicitly — see
  `.rules/auth.md`.
- When a refactor touches clone, routing, or traversal behavior, compare
  against the pre-refactor behavior before accepting the new result.
- Preserve existing repo-tree passthrough and ownership semantics unless
  intentionally changing the contract.
- **Providers must project all data they have already fetched.** If a
  handler has an upstream payload in hand, emit every sibling file and
  child that can be derived from it instead of returning only the
  requested field and forcing later refetches. See `.rules/provider-sdk.md`
  and `.rules/gotchas.md`.

## Design judgment

- Prefer the simpler end-to-end flow, not the purer local abstraction.
- Bias toward single-phase designs over multi-phase orchestration on the
  hot path.
- Keep data near the point where it is naturally produced and immediately
  consumed; split it into a second mechanism only when that separation
  buys something concrete.
- Do not defend abstraction boundaries that add complexity in the common
  case.
- Once the direct path exists, remove bridge-style dispatch layers and
  other transitional glue instead of letting them harden into
  architecture.

## Protocol and contract guardrails

- Reuse source-of-truth terms. Do not invent new names for public surfaces
  unless the rename is explicit.
- Keep public contracts at the right layer. Host internals must not leak
  into SDK/WIT naming or semantics.
- Do not reuse an existing abstraction if it changes the behavior model.
  Semantic fit matters more than code reuse.
- For protocol changes, write the exact interaction trace first and reject
  extra hops on hot paths.
- If something is conceptually one-way, stop before making it
  `await`-shaped. Fix the boundary instead of forcing it through
  request/response machinery.

## Mutation protocol

Mutations are not implemented yet. The accepted-direction design is
`design/mutations-via-git.md`: the mounted scope is itself a Git
repository; local edits stay local until `git add` / `git commit` /
`git push`, and the push is reconciled through provider
`plan-mutations` / `execute`.

Do not make projected issue/PR files directly writable as an implicit
mutation mechanism.

---

## Design status convention

Designs carry different maturity. Each design doc has a one-line `Status:`
field near the top, drawn from this set:

| State                       | Meaning                                                                |
|-----------------------------|------------------------------------------------------------------------|
| `proposed`                  | Written, awaiting decision. Not yet a commitment.                      |
| `accepted`                  | Decision made. This is the plan; partial or full implementation may follow. |
| `implemented on <branch>`   | Fully realized in code on the named branch (or `main`).                |
| `superseded by <path>`      | No longer current; named doc replaces it. Kept for context.            |
| `historical`                | Not current and not directly replaced. Useful only as background.      |

Refinements like `draft` (in flight) or `ready to implement` are allowed,
but the primary state must be one of the above. Working materials such as
implementation prompts are not designs and should be marked accordingly.

The aggregate index at `docs/design/README.md` lists every design doc and
its current status. Update both the doc's `Status:` line and the index
when status changes.
