# Contract docs index

Status: current-contract
Owns: the agent-facing map for binding `omnifs` rules.

## Read when

Read this first to choose a contract. `AGENTS.md` is always loaded; this index
routes the task without loading every contract.

## Rules

| If touching | Read |
|---|---|
| Trust, byte boundary, provider authority, auth, credentials, sandbox claims | `10-system.md` |
| Provider SDK, provider macros, objects, routes, WIT, metadata, provider config, endpoints | `20-provider-sdk.md` |
| Projection tree, cache, attrs, listing, lookup, traversal, learned sizes, live growth | `30-projection-tree.md` |
| FUSE, NFS, mount protocol behavior, filesystem state, protocol replies | `40-filesystems.md` |
| CLI, daemon, typed local control protocol, filesystem runtimes, profile bootstrap, daemon state, dev home | `50-control-plane.md` |
| CI, validation commands, provider artifacts, generated schema, docs checks | `60-build-validation.md` |

Documentation types:

- `AGENTS.md`: always-loaded universal rules and operating entry point.
- `docs/contracts/`: binding task-area rules.
- `docs/architecture/`: current model and rationale, loaded only for subsystem
  context.

## Must not

- Turn contracts into broad explanatory essays.
- Split a contract unless the split avoids loading irrelevant rules.
- Keep a stale mixed doc path as alternate authority.

## Code

- `AGENTS.md`
- `scripts/ci/check-doc-contracts.sh`
- `scripts/ci/check-doc-links.sh`

## Validation

- `just docs-check`
