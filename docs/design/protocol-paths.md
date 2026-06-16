# Protocol paths

Scope: `crates/omnifs-wit/wit/provider.wit`, `crates/omnifs-sdk`, `crates/omnifs-host`, providers.

omnifs has a single path space. A protocol path is an absolute,
forward-slash-delimited string that satisfies the invariants below. It
is the only path shape that crosses the WIT boundary, and the only
shape stored in host caches.

## Invariants

1. **Always absolute.** A path begins with `/`. The empty string is not
   a valid path anywhere.
2. **Root is `/`.** The root directory has the path `/`. It is the only
   path that ends in `/`.
3. **No trailing slash.** A non-root directory path is `/foo/bar`, not
   `/foo/bar/`.
4. **Segments are non-empty.** `/foo//bar` is invalid. Each segment is
   a non-empty UTF-8 string that does not itself contain `/`.
5. **No `.` or `..` segments.** Path normalisation is not deferred to
   consumers.

A path that violates any invariant is a provider contract error at the
WIT boundary and a programming error inside the host or SDK.

## `Path` type

All path arithmetic goes through one newtype:

```rust
/// A protocol path. Always starts with `/`. Never has a trailing `/`
/// except the root path itself. Never contains `//` or `.`/`..` segments.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Path(String);

impl Path {
    pub const ROOT: &str = "/";

    /// Construct from a string already known to satisfy the invariants.
    /// Use at trust boundaries (host-minted paths re-handed to the host,
    /// WIT deserialization where the validator vouched for the encoding).
    pub fn from_validated(s: impl Into<String>) -> Self;

    /// Parse a candidate path, checking invariants.
    pub fn parse(s: &str) -> Result<Self, ParseError>;

    /// Join a single child name. `name` must not contain `/`.
    pub fn join(&self, name: &str) -> Result<Self, ParseError>;

    /// Parent. `/foo/bar` → `/foo`, `/foo` → `/`, `/` → `None`.
    pub fn parent(&self) -> Option<Path>;

    /// Basename. `/foo/bar` → `"bar"`, `/foo` → `"foo"`, `/` → `""`.
    pub fn name(&self) -> &str;

    pub fn is_root(&self) -> bool;

    /// Segments after the leading `/`, in order.
    pub fn segments(&self) -> impl Iterator<Item = &str>;

    /// Segment-boundary-safe prefix check. `/foo/bar` is a prefix of
    /// `/foo/bar/baz` but NOT of `/foo/barbecue`.
    pub fn has_prefix(&self, prefix: &Path) -> bool;

    /// Strip a prefix. None when `prefix` is not a prefix of `self`.
    pub fn strip_prefix(&self, prefix: &Path) -> Option<Path>;

    pub fn as_str(&self) -> &str;
}
```

```rust
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("empty path")]
    Empty,
    #[error("path is not absolute: {0:?}")]
    MissingLeadingSlash(String),
    #[error("double slash in path: {0:?}")]
    DoubleSlash(String),
    #[error("trailing slash on non-root path: {0:?}")]
    TrailingSlash(String),
    #[error("empty path segment")]
    EmptySegment,
    #[error("path contains `.` or `..` segment: {0:?}")]
    RelativeSegment(String),
    #[error("name segment contains `/`: {0:?}")]
    SlashInSegment(String),
    #[error("path segment is not valid UTF-8")]
    NonUtf8Segment,
}
```

## One type, not two

The design intentionally uses a single newtype rather than a pair like
`AbsolutePath` + `BarePath`. There is only one path space; encoding the
absent space in the type would carry zero information.

Provider authors get `Path::ROOT` as the implicit base. Routing matches
against absolute segments. The macros and dispatch layer never see a
bare form.

## WIT boundary

Every path-shaped `string` field in `crates/omnifs-wit/wit/provider.wit`
(there is no `path` type alias; `parent-path` and `path` args are bare
`string`) satisfies the invariants. The host calls `Path::parse` on every
path string crossing the WIT boundary in either direction; a `ParseError`
becomes a provider error and fails the operation.

The validation cost is one O(len) scan per path. A typical browse
op carries 1–2 paths, so the cost is negligible. Paths the host already
minted (cache keys, route-matched paths) skip validation via
`Path::from_validated`.

## Hot-path cost

`Path` wraps `String`. Per-callout and per-FUSE-op sites do at most one
allocation per call (the `format!` inside `join`). If profiling later
shows allocation pressure, swap the inner to `Arc<str>` or `SmolStr`
behind the same API.

## Cache key encoding

The view cache (non-durable `view.redb`, deleted and recreated on every startup) and object cache (durable `object.redb`) both key on the path string. `Path` serialises through serde to the inner `String`, so on-disk records remain byte-identical to a naked string of the same value. A `SCHEMA_VERSION` bump is only needed if a postcard fixture comparison shows otherwise.

## Provider ergonomics

Providers build paths with `Path::parse(format!("/foo/{id}"))`. A
`path!("/foo/{}", id)` macro is plausible sugar but is not part of the
contract; add it only if friction emerges.

Providers must not return bare relative paths. The dispatcher does not
prepend `/` for them. Path-shaped fields in WIT records (lookup-entry,
dir-listing, fs-write, canonical-store anchor) all carry absolute paths.
