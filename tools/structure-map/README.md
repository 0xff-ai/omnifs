# structure-map

`structure-map` builds a Rust source outline for agent cleanup work. It scans
files or directories, emits symbols, fields, variants, impl methods, free
functions, and a small set of evidence-backed cleanup signals.

This first version is intentionally offline. It parses source with `syn` and
does not start `rust-analyzer`, so call hierarchy is not included yet.

```bash
cargo run --manifest-path tools/structure-map/Cargo.toml -- crates/omnifs-host/src/cache.rs
cargo run --manifest-path tools/structure-map/Cargo.toml -- crates/omnifs-host --format tree
```

Signals are heuristic and should be treated as prompts for agent judgment, not
as refactor instructions.

When signals activate, the output also includes a separate `recommendations`
section. Recommendations are grouped by heuristic kind and emitted once per
activated kind, never inline with individual findings.

Current source-only signals:

- receiver clusters
- parameter clusters
- static associated helper clusters
- receiverless associated namespaces
- free-function ownership clusters
- DTO conversion free functions
- passive enum switchboards
- namespace-redundant public type names
- enum match ladders
- repeated callee sequences
- parse/build/load calls inside loops
- call-site choreography
- conversion/display/serde impls that do IO
- IO/presentation mixing
- presentation work in effectful functions
- DTO/domain naming mix
- primitive obsession
- validation bypass risk
- fan-out hotspots

Signals that require LSP call/reference edges are not emitted yet: incoming-call
hotspots, public-but-internal items, one-caller helpers, and test-only shape
pressure.
