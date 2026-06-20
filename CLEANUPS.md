# Cleanup backlog

This records the 2026-06-20 de-entropization audit for `omnifs-cli`,
`omnifs-daemon`, and `omnifs-host`.

The bar for these refactors is caller shape: each cleanup should remove
policy-bearing coordination from command paths, daemon setup, or host hot paths.
Prefer behavior on the type that owns the invariant over forwarding helpers,
bridge modules, and duplicated switchboards.

## Status

First slice completed in `refactor(lifecycle): centralize daemon launch context`:

- Items 1, 2, and 4: `omnifs up` now goes through a command-owned
  `Launcher`, uses configured runtime selection only, and keeps the command
  body focused on `up` presentation.
- Item 10: CLI daemon probing now returns
  `DaemonControlState::{Absent,Sick,Incompatible,Compatible}` instead of
  spreading compatibility control flow through callers.
- Item 11: daemon startup now uses `DaemonContext` for workspace layout,
  mount point, frontend choice, backend/materialization mode, process facts,
  control listener, and NFS options.
- Items 15 and 16: host `Dirs` was replaced by `HostContext`, and
  `ProviderRegistry` delegates mount instances, timers, fingerprints, and the
  reconcile lock to `MountSupervisor`.

Remaining work in those items:

- Item 1 still has multiple runtime-mode types. The first slice reduced call
  site leakage but did not fully collapse `ConfiguredBackend`,
  `LaunchBackend`, `DaemonBackend`, and `MaterializationMode`.
- Item 2 still leaves `omnifs dev` on the lower-level `LaunchSpec` path,
  because dev has fixture and sandbox setup that should be split deliberately.
- Item 15 moved lifecycle state into `MountSupervisor`, but reconcile planning
  still lives in `registry.rs`; the next slice should decide whether planning
  also belongs behind the supervisor interface.

## Next batch

The next parallel batch should be:

1. **Status snapshot lane:** items 5, 10, 13, and 14. Collapse CLI/daemon
   status shaping around a single snapshot/readiness model.
2. **Provider catalog lane:** items 4 and 8. Deepen `Workspace`/`ProviderCatalog`
   so provider templates become an indexed catalog surface instead of repeated
   lookup helpers.
3. **Materialization lane:** items 3, 17, and 18. Move Docker preopen policy
   toward typed materialized mounts and split effect/read materialization before
   touching the archive extractor.

## Start here

The highest-leverage starting set is:

1. Unify runtime mode into one type family instead of carrying
   `ConfiguredBackend`, `LaunchBackend`, `DaemonBackend`, and
   `MaterializationMode` as overlapping concepts.
2. Make launch lifecycle a real module owned by something like
   `Launcher { workspace, catalog, daemon }`.
3. Make CLI `Workspace` the command session for config, provider catalog, daemon
   client, and mount paths.
4. Type daemon control state as `Absent`, `Sick`, `Incompatible`, and
   `Compatible` instead of scattering probe booleans and ad hoc status checks.
5. Make daemon construction use an explicit `DaemonContext`.
6. Move reconcile side effects into a `MountSupervisor`.
7. Remove the host `Dirs` bag and have host code consume daemon workspace
   context or a real host context.

## Opportunities

1. **Unify runtime mode.**
   Collapse the current runtime vocabulary into one domain model. Docker and
   native should be launch targets or daemon placements, not separate enums that
   every caller has to translate between.

2. **Make launch lifecycle one module.**
   `omnifs up` should read as one lifecycle operation: resolve workspace,
   resolve launch target, prepare providers, start or reclaim the daemon, wait
   for readiness, and report status. A `Launcher` can own the policy instead of
   spreading it across commands, backend helpers, launch records, and runtime
   adapters.

3. **Move Docker preopen policy off mount config.**
   Mount config should describe desired projections. Docker-specific preopen
   materialization should live in the launch/materialization path, probably
   through a typed `MaterializedMount`.

4. **Make `Workspace` the fuller command session.**
   CLI commands repeatedly need config, provider catalog, daemon client, and
   mount directories. Put that ownership on `Workspace` so commands do not
   re-derive adjacent context by hand.

5. **Collapse status DTO sprawl.**
   The CLI should have one `StatusSnapshot` or equivalent command-facing view
   for daemon version, launch state, frontend state, mount state, and auth
   readiness. Text and JSON should project from that view.

6. **Put auth status on auth types.**
   Auth status is currently high-entropy because command code shapes readiness,
   labels, and JSON externally. Move that presentation into an `AuthStatusView`
   under the auth module.

7. **Centralize credential target derivation.**
   CLI auth commands and host credential lookup should share one policy object
   for deriving credential targets from mounts, providers, accounts, and config.

8. **Deepen provider templates into an index.**
   Provider template discovery should be a parsed `ProviderTemplates` index
   built from the catalog, not repeated directory scans and lookup helpers.

9. **Extract doctor probes into probe adapters.**
   `doctor` should assemble probe results from adapters instead of mixing
   collection, diagnosis, terminal rendering, and daemon control decisions.

10. **Type daemon control state.**
    Replace boolean readiness and compatibility checks with a control-state enum
    that owns next actions and display. Commands should not have to remember
    which combination of errors means absent, sick, incompatible, or compatible.

11. **Make daemon context explicit.**
    Daemon startup should receive one context with workspace layout, config,
    catalog or registry, frontend selection, and process metadata. Avoid
    rebuilding context inside `app`, `server`, and `frontends`.

12. **Promote the frontend supervisor.**
    Frontend startup, health, and teardown should be owned by one supervisor
    rather than spread across daemon app setup and frontend helpers.

13. **Replace boolean readiness with `Readiness`.**
    Use a typed readiness state such as `Starting`, `Serving`, and `Failed` for
    daemon/frontends. This lets waiting, status, and errors share one model.

14. **Centralize daemon status snapshot.**
    The daemon should construct one status snapshot internally and serve it to
    clients. CLI status should not have to reconstruct daemon facts from
    several endpoints or probes.

15. **Move reconcile side effects into `MountSupervisor`.**
    Provider registry reconcile currently mixes scanning, loading, diffing,
    starting, stopping, and reporting. A supervisor can own mount lifecycle and
    leave registry/catalog types to model provider availability.

16. **Remove host `Dirs`.**
    `Dirs` is another path bag. Replace it with daemon workspace context or a
    host context that makes the specific owned directories explicit.

17. **Split materialization internals.**
    Separate effect materialization from browse/read materialization. The host
    can then keep effect commits, cache writes, archive trees, and read paths
    distinct without pushing that distinction onto callers.

18. **Make object path indexing a type.**
    The object materialization path needs an `ObjectPathIndex` or equivalent for
    file path lookup, tree membership, and sibling derivation instead of repeated
    ad hoc path maps.

19. **Shrink the runtime cache facade.**
    Replace broad cache access through `Runtime` with a narrower
    `MountCacheAccess` or equivalent handle for the operations hot paths
    actually need.

20. **Replace the archive Wasm extractor.**
    Remove the archive provider/tool component and materialize archives in the
    host while preserving the existing safety properties: reserved limits, path
    sanitization, temp-dir publish, and tree-ref cache.
