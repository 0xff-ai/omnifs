# Path arithmetic unification

## Goal

omnifs paths today live in two parallel encodings: the host/FUSE side
uses absolute paths (`/foo/bar`), and the provider/WIT side uses bare
relative paths (`foo/bar`). Empty parents are encoded inconsistently
(`""` vs `"/"` vs `String::new()`), and at least 12 helpers across 5
crates do near-identical arithmetic.

This work unifies the two spaces. All paths become absolute strings
with a leading `/`. The root path is `/`, never `""`. There is one
shared path module, and every site uses it.

## Target invariants

After the cleanup, every protocol path the host or a provider produces
satisfies:

1. **Always absolute.** Every path is `/`-prefixed. There is no
   "bare" form. The empty string is not a valid path anywhere.
2. **Root is `/`.** The root directory has the path `/`, not `""`.
3. **No trailing slash.** A directory path is `/foo/bar`, not
   `/foo/bar/`. The root is the only path that ends in `/`.
4. **Segments are non-empty.** `/foo//bar` is invalid; `/foo/` is
   invalid (except for the root). Path construction collapses or
   rejects empty segments.
5. **Single canonical helper module.** All path arithmetic goes
   through `omnifs_paths` (a new crate, or `omnifs-sdk/src/path.rs`
   if we prefer to avoid a new crate — pick one in the design pass).

## Current state inventory

Capturing the catalog so this document is executable without
re-running the analysis.

### Existing helpers to replace

| File:line | Name | What it does today |
|---|---|---|
| `crates/host/src/fuse/mod.rs:70` | `join_child_path(parent, name)` | bare-space join, empty parent → bare name |
| `crates/host/src/runtime/mod.rs:405` | `absolute_mount_path(path)` | relative → absolute, empty → `/` |
| `crates/host/src/runtime/effects.rs:84` | `split_projected_path(path)` | rsplit_once on `/`, root → `("", name)`, returns Option |
| `crates/host/src/runtime/invalidation.rs:142` | `parent_child_for_notify(path)` | same split, owned parent, rejects trailing slash |
| `crates/host/src/path_prefix.rs:1` | `path_prefix_matches(prefix, path)` | segment-boundary-safe prefix check |
| `crates/cli/src/mount_tree.rs:128` | `path_tail(path)` | basename, `/` → `/` |
| `crates/omnifs-mount-schema/src/lib.rs:756` | `join_absolute_path(segs)` | joins `&[&str]` with `/` leader, empty → `/` |
| `crates/omnifs-sdk/src/handler.rs:1723` | `to_absolute_path(path)` | absolute conversion, normalises `/` input |
| `crates/omnifs-sdk/src/handler.rs:1733` | `join_absolute_path(parent, child)` | absolute join, parent `/` → `/{child}` |
| `crates/omnifs-sdk/src/handler.rs:1741` | `join_provider_path(parent, child)` | bare join, strips all slashes from parent |
| `crates/omnifs-sdk/src/handler.rs:1750` | `child_name(path)` | basename, `/` → `None` |
| `crates/omnifs-sdk/src/handler.rs:1758` | `split_parent_name(path)` | borrowed-parent split |

### Inline duplicate JOIN sites

These are the same `if parent.is_empty() { name } else { format!("{parent}/{name}") }`
pattern, inlined:

- `crates/host/src/runtime/browse_pipeline.rs:19-23` (in `lookup_child`)
- `crates/host/src/runtime/browse_pipeline.rs:177-181` (in `cache_projection_batch`)
- `crates/host/src/runtime/op.rs:179-182` (in `validate_handoff_path`)

### Provider-side path construction

Provider crates currently build bare relative paths via `format!()`:

- `providers/github/src/issues.rs:94, 117, 127`
- `providers/github/src/pulls.rs:97`
- `providers/arxiv/src/recent.rs:355, 376, 414, 416`

After the unification, these become `/`-rooted strings. The macros and
SDK handler dispatch (which today strips a leading slash via
`trim_start_matches('/')` and then splits) become a straight split.

### Sites doing slash trim / normalization that disappear

These exist only to translate between the two encodings:

- `crates/omnifs-sdk/src/handler.rs:966` — `path.trim_start_matches('/').split('/')`
- `crates/omnifs-sdk-macros/src/handler_macro.rs:784, 791` — same
- `providers/github/src/types.rs:36` — same
- `providers/github/src/issues.rs:117, 127` — `base.trim_end_matches('/')`

## The new path module

### Shape

One module exporting two free functions and a single newtype. Keep it
small. We are not introducing two newtypes (Absolute vs Bare) because
the whole point of the cleanup is that there is only one space.

```rust
// crates/omnifs-sdk/src/path.rs (or new crate `omnifs-paths`)

/// A protocol path. Always starts with `/`. Never has a trailing `/`
/// except the root path itself. Never contains `//` or `.` / `..` segments.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Path(String);

impl Path {
    /// The root path, `/`.
    pub const ROOT: &str = "/";

    /// Construct from a string that is already known to satisfy the invariants.
    /// Use this at trust boundaries (after `parse`, after `WIT` deserialization
    /// if the WIT validator vouches for the encoding).
    pub fn from_validated(s: impl Into<String>) -> Self { ... }

    /// Parse a candidate path, checking invariants. Returns an error if the
    /// input has `//`, trailing slash (other than root), `.`/`..`, or any
    /// other shape that violates the invariants. Accepts both `/foo/bar`
    /// and `foo/bar` (the latter is rooted by adding `/`).
    pub fn parse(s: &str) -> Result<Self, PathParseError> { ... }

    /// Join a single child name onto this path.
    /// `name` must not contain `/`. Errors otherwise.
    pub fn join(&self, name: &str) -> Result<Self, PathParseError> { ... }

    /// Parent of this path. `/foo/bar` → `/foo`. `/foo` → `/`. `/` → None.
    pub fn parent(&self) -> Option<Path> { ... }

    /// Basename. `/foo/bar` → `"bar"`. `/foo` → `"foo"`. `/` → `""`.
    pub fn name(&self) -> &str { ... }

    /// Whether this is the root path.
    pub fn is_root(&self) -> bool { self.0 == Self::ROOT }

    /// Segments after the leading `/`, in order.
    /// `/foo/bar` → ["foo", "bar"]. `/` → empty.
    pub fn segments(&self) -> impl Iterator<Item = &str> { ... }

    /// Segment-boundary-safe prefix check. `/foo/bar` is a prefix of
    /// `/foo/bar/baz` but NOT of `/foo/barbecue`.
    pub fn has_prefix(&self, prefix: &Path) -> bool { ... }

    /// Strip a prefix. Returns None if `prefix` is not a prefix of `self`.
    /// `/foo/bar/baz`.strip_prefix(`/foo`) → Some(Path::parse("/bar/baz")).
    /// `/foo`.strip_prefix(`/foo`) → Some(Path::ROOT).
    pub fn strip_prefix(&self, prefix: &Path) -> Option<Path> { ... }

    /// View as a `&str` (always starts with `/`).
    pub fn as_str(&self) -> &str { &self.0 }
}

#[derive(Debug, thiserror::Error)]
pub enum PathParseError {
    #[error("empty path")]
    Empty,
    #[error("double slash in path: {0:?}")]
    DoubleSlash(String),
    #[error("trailing slash on non-root path: {0:?}")]
    TrailingSlash(String),
    #[error("path contains `.` or `..` segment: {0:?}")]
    RelativeSegment(String),
    #[error("name segment contains `/`: {0:?}")]
    SlashInSegment(String),
}
```

### Why not two types

A two-type design (`AbsolutePath` and `BarePath`) would track the space
in the type, but our goal is to eliminate the bare space. After the
cleanup there is one space, so one type. Provider authors get
`Path::ROOT` (`/`) as the implicit base, and routing matches against
absolute segments.

### Hot-path concern

`Path` wraps `String`. The hot sites (per-callout, per-FUSE-op) do at
most one allocation per call (the format! in `join`). That matches
today's cost. If profiling later shows allocation pressure, swap the
inner to `Arc<str>` or `SmolStr` behind the same API.

## Migration plan

Land in this order. Each step is a separate commit. `just check` must
pass after each.

### Step 1: Introduce the module

Create `crates/omnifs-sdk/src/path.rs` (or new `omnifs-paths` crate —
pick before this step). Implement `Path` + `parse` + `join` +
`parent` + `name` + `is_root` + `segments` + `has_prefix` +
`strip_prefix` + `PathParseError`.

Tests:
- Round-trip every valid form (`/`, `/foo`, `/foo/bar`).
- Reject every invalid form (`""`, `foo`, `/foo/`, `/foo//bar`, `/foo/.`, `/foo/..`, `/foo/bar/baz/`).
- Join: `Path::parse("/foo").unwrap().join("bar").unwrap().as_str() == "/foo/bar"`.
- Join rejects `name` containing `/`.
- Parent: `/foo/bar` → `Some(/foo)`; `/foo` → `Some(/)`; `/` → `None`.
- Strip prefix: `/foo/bar/baz`.strip_prefix(`/foo`) → `/bar/baz`; `/foo`.strip_prefix(`/foo`) → `/`.

Nothing else changes in this commit. No call sites migrate yet.

### Step 2: WIT contract

Document in `wit/provider.wit` that path strings carried by op
arguments and op results must satisfy `Path`'s invariants:

```
/// Protocol path. Always begins with `/`. The root is `/`.
/// Directory paths have no trailing `/` except the root.
/// Segments are non-empty; `.` and `..` are not permitted.
type path = string;
```

No generated code changes here, just docs. The host enforces the
invariant by passing every path through `Path::parse` at the WIT
boundary on both sides.

### Step 3: Migrate the host runtime

In `crates/host/src/runtime/`:

- Replace `absolute_mount_path` with `Path::parse`.
- Replace `split_projected_path` and `parent_child_for_notify` with
  `Path::parent` + `Path::name`. Update their call sites to thread the
  `Path` type through rather than re-splitting strings.
- Replace the three inline JOIN sites (browse_pipeline.rs:19-23,
  browse_pipeline.rs:177-181, op.rs:179-182) with `Path::join`.
- The current `RuntimeError::ProviderProtocol` error path absorbs
  any `PathParseError` raised at the WIT boundary.

The browse path (`ProviderRuntime::lookup_child`, `list_children`,
`read_file`, etc.) takes `&str` from FUSE and `Path::parse`s once at
entry. Internal state stores `Path`, not `String`.

### Step 4: Migrate the SDK handler dispatch

In `crates/omnifs-sdk/src/handler.rs`:

- Delete `to_absolute_path`, `join_absolute_path`, `join_provider_path`,
  `child_name`, `split_parent_name`. All callers route through `Path`.
- `match_bind_with` (line ~966) receives a `&Path` and calls
  `Path::segments()` instead of `path.trim_start_matches('/').split('/')`.
- The mount-schema's `join_absolute_path` (lib.rs:756) becomes
  `Path::from_segments(&[&str]) -> Path`, owned by the path module.

### Step 5: Migrate the macros

`crates/omnifs-sdk-macros/src/handler_macro.rs` generates the dispatch
that currently does `path.trim_start_matches('/').split('/')`. Update
the macro to emit `path.segments()` (operating on `&Path`).

### Step 6: Migrate the providers

Update `providers/github/src/{issues.rs,pulls.rs,types.rs}` and
`providers/arxiv/src/recent.rs` to return `Path` (or strings that round-trip
through `Path::parse`). Drop every `trim_end_matches('/')` and
`trim_start_matches('/')` site — those vanish because the format is
uniform.

### Step 7: Migrate the FUSE layer

`crates/host/src/fuse/mod.rs::join_child_path` is gone. The FUSE layer
parses the entry name into a path segment and calls `parent.join(name)`.
Other FUSE-side conversions (e.g. `path_tail` in
`crates/cli/src/mount_tree.rs`) use `Path::name`.

### Step 8: Cache key encoding

The L2 cache currently keys on the path string. The `Path` type
serializes to its `as_str()` representation, so on-disk records stay
compatible — but verify with a postcard fixture compare. If the
postcard encoding differs (e.g. because `Path` is a tuple struct and
`String` is naked), bump `SCHEMA_VERSION` and document the
invalidation just like Phase 8.2 did.

### Step 9: Delete the migration cruft

After all sites are on `Path`:

- Remove `crates/host/src/path_prefix.rs` (folded into `Path::has_prefix`).
- Remove `crates/cli/src/mount_tree.rs::path_tail`.
- Confirm no callers of any of the 12 helpers listed above remain.

## Edge cases and migration risks

### Empty parent / bare-name compatibility

The existing host code passes `""` as the parent for root-level
lookups. After migration, root-level lookup is `Path::ROOT.join(name)`.
Audit every call site that constructs a path from `(parent, name)`
where `parent` could be empty — confirm it switches to `Path::ROOT`.

### WIT-boundary validation cost

`Path::parse` runs on every path string crossing the WIT boundary.
For a typical browse op, that's 1-2 paths per call. Cost: O(len) scan.
This is acceptable. If profiling shows it's hot, add a
`Path::from_validated` fast path for paths the host already minted.

### Provider authoring ergonomics

Providers today build paths with raw `format!()`. After migration,
they use `Path::parse` or a `path!("/foo/bar/{}", id)` macro if we
want sugar. Don't ship the macro in step 1; let providers use
`Path::parse` first and add sugar only if friction emerges.

### Hashbrown vs std HashMap

CLAUDE.md notes that providers should use `hashbrown::HashMap` for
internal state. The `Path` newtype implements `Hash` via the inner
`String`, which works with both maps. No change needed.

### Tests

Path-handling regression tests in `runtime/op.rs::attr_contract_tests`
and any test that constructs paths manually (e.g.
`crates/host/tests/auth_test.rs`, the `callout_tracing` snapshot test)
need to either go through `Path::parse` or be updated to use the
absolute form.

## Definition of done

- `Path` newtype shipped in a single module with parse, join, parent,
  name, segments, strip_prefix, has_prefix.
- Every site listed in "Current state inventory" migrated or deleted.
- `just check` green at every step.
- No remaining `trim_start_matches('/')` or `trim_end_matches('/')`
  calls on protocol paths (search proves it).
- No remaining `format!("{}/{}", parent, name)` joins on protocol paths.
- The WIT comment in `wit/provider.wit` documents the invariants.
- Cache schema either round-trips unchanged or bumps with an
  invalidation test (mirroring Phase 8.2).

## Out of scope for this work

- Renaming `path_prefix_matches` callers in non-runtime crates that
  don't operate on protocol paths.
- The mount tree CLI's `path_tail` if it operates on user-facing
  CLI strings rather than protocol paths (audit before deleting).
- Performance work on `Path` (switching to `Arc<str>`, SmolStr).
