# Contract docs index

Status: current-contract
Owns: the agent-facing map for binding `omnifs` rules.

## Read when

Read this first when deciding which contract applies. Do not load every contract by default. `AGENTS.md` is the always-loaded router; the files here are task-area rules.

## Rules

| If touching | Read |
|---|---|
| Trust, byte boundary, provider authority, auth, credentials, sandbox claims | `10-system.md` |
| Provider SDK, provider macros, objects, routes, WIT, metadata, provider config, endpoints | `20-provider-sdk.md` |
| Projection tree, cache, attrs, listing, lookup, traversal, learned sizes, live growth | `30-projection-tree.md` |
| FUSE, NFS, mount protocol behavior, frontend state, protocol replies | `40-frontends.md` |
| CLI, daemon, REST API, runtime modes, workspace layout, mount delivery, dev home | `50-control-plane.md` |
| CI, validation commands, provider artifacts, generated OpenAPI/schema, docs checks | `60-build-validation.md` |

Documentation types:

- `AGENTS.md`: always-loaded operating guide.
- `docs/contracts/`: binding rules by task area.
- `docs/architecture/`: current explanatory model and rationale.
- `docs/future/`: proposals and non-current direction.

## Must not

- Do not turn contracts into broad explanatory essays.
- Do not split a contract file unless agents can avoid loading irrelevant rules because of the split.
- Do not keep a stale mixed doc path as alternate authority.

## Code

- `AGENTS.md`
- `scripts/ci/check-doc-contracts.sh`
- `scripts/ci/check-doc-links.sh`

## Validation

- `just docs-check`
