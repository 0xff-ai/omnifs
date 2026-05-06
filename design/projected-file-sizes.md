# Projected file sizes: honest stat, direct_io, and lazy resolution

Status: draft, for review
Scope: `wit/provider.wit`, host FUSE layer, SDK projection API, providers
Branch: design/projected-file-sizes

## Problem

A projected file's real length is unknown until the provider serves the
read. The current SDK reports a fixed 256 MiB placeholder for every
projected file whose size is not pre-computed. The kernel uses this
value for two distinct purposes that have been collapsed into one:

1. **Read termination**: the kernel caps `read` requests at the file's
   reported size, then treats a short read as EOF. A too-small
   placeholder truncates real payloads; a too-large one is fine here.
2. **Userspace reporting**: `ls -l`, `du`, `find -size`, progress bars,
   and pre-allocated download buffers consume the stat size as truth.
   A too-large placeholder makes these tools report nonsense (a
   100-paper directory shows as 25 GiB; `du -sh` over a mounted root
   reports terabytes; `head -c $((size))` allocates a 256 MiB buffer).

256 MiB is the current compromise: large enough to not truncate any
realistic provider download, small enough to read as `256M` rather than
`1.0G`. The compromise is unsound in both directions. Some sources
(arXiv source tarballs with embedded TeX figures, github release
archives) can exceed 256 MiB and silently truncate. Even when nothing
truncates, the reported sizes are fiction.

This document proposes decoupling the two concerns.

## Goals

- Read termination must not depend on a stat-size guess.
- Stat-size must reflect either the real length or a sentinel that
  tooling can treat as "unknown," not a guess presented as truth.
- Providers that can compute a size cheaply (from API metadata,
  Content-Length headers, or fully materialized payloads) should be
  able to report it.
- The SDK API must distinguish "I know the size" from "I do not."
- macOS (macFUSE) and Linux behavior must both be considered, since
  omnifs runs in both environments.

## Non-goals

- Probing upstream with HEAD just to learn a size on `stat`. The
  per-stat cost is unacceptable, and many endpoints (gzipped PDFs,
  chunked responses, computed projections) would not return a useful
  Content-Length anyway.
- A virtual-filesystem-wide size cache layer. Sizes learned from a
  provider read flow through the existing projection cache like any
  other content.
- Removing the eager-projection 64 KiB ceiling on `file_with_content`.
  That bound is independent and stays as is.

## Design

### Three sources of size truth

A projected file is in one of three size states at any moment:

1. **Known-from-projection**: the projection handler materialized the
   bytes. Size is `bytes.len()`. Already the case for
   `file_with_content`.
2. **Known-from-metadata**: the projection handler did not materialize
   bytes but holds a size from some other source (an API field, a
   prior cached fetch, a content-length header). Currently
   expressible only via `file_with_stat(name, FileStat { size })`,
   which providers rarely reach for.
3. **Unknown-until-read**: the projection handler does not know the
   size and has no cheap way to compute it. This is the case the
   placeholder exists to paper over.

Cases 1 and 2 can produce an honest `st_size`. Case 3 cannot, and is
where the design has to make a choice.

### Case 3: unknown-until-read

The kernel-side answer is `direct_io`. When a file is opened with the
`FOPEN_DIRECT_IO` flag in the FUSE `open` reply, the kernel:

- Does not cap reads at `st_size`. Reads are passed through verbatim
  and the FUSE driver returns whatever bytes it has; a short read
  signals EOF.
- Does not use the page cache for this file. Every read is a syscall
  round trip to userspace. Writes (irrelevant here) bypass the cache
  too.
- May still allow `mmap` if the kernel supports
  `FUSE_DIRECT_IO_ALLOW_MMAP` (Linux 6.x mainline; macFUSE varies).
  Tools that mmap large projected files on older kernels will fail;
  this is acceptable for the known-unknown case.

With `direct_io`, the stat size is decoupled from read termination
and becomes a pure reporting field. The honest value is 0. `ls -l`
reports `0`, which is correct: zero bytes are knowable without
fetching. `du` over a directory of unknown-size projected files
reports 0 bytes used, which is also correct: nothing has been
downloaded. Once a read completes and the projection cache holds the
real bytes, the host can update the inode's cached size and notify
the kernel via `fuse_lowlevel_notify_inval_inode`, so the next
`stat` returns the real length.

### SDK API

Replace `Projection::file(name)` and `file_with_stat(name, stat)` with
an explicit `Size` type:

```rust
// crates/omnifs-sdk/src/handler.rs

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Size {
    /// The provider knows the exact byte length of this file. Stat
    /// reports it directly; reads terminate when the kernel reaches
    /// it.
    Exact(NonZeroU64),

    /// The provider does not know the size cheaply. The host opens
    /// the file with `direct_io`, reports `st_size = 0` until a read
    /// resolves the real length, and updates the kernel inode after
    /// the first successful read.
    Unknown,
}

impl Projection {
    pub fn file(&mut self, name: impl Into<String>, size: Size) { ... }

    pub fn file_with_content(&mut self, name: impl Into<String>, bytes: impl Into<Vec<u8>>) { ... }
}
```

`file_with_content` keeps its current semantics (size derived from
`bytes.len()`) and is the right call when the projection materializes
small payloads inline.

The placeholder constant `DEFAULT_FILE_SIZE_BYTES` and
`FileStat::placeholder` go away. `FileStat` collapses to a single
`Exact` variant or merges into `Size`.

The macro layer (`#[file]`, `#[dir]`) gains an attribute argument so
exact handlers can declare a size at registration time when it is
known statically:

```rust
#[omnifs_sdk::file("metadata.json", size = "exact")]  // size derived from returned bytes
#[omnifs_sdk::file("paper.pdf", size = "unknown")]    // direct_io, lazy resolution
```

Default for `#[file]` is `unknown`, since that is the conservative
choice; handlers that return materialized bytes opt into `exact` (and
the SDK will assert the returned bytes are non-empty at runtime).

### WIT changes

`dir-entry.size: option<u64>` becomes a richer variant so the host can
distinguish "unknown, use direct_io" from "directory entry, no size
applies":

```wit
variant entry-size {
    /// Directory entries and files whose size the provider has not
    /// computed. For files, the host opens with direct_io and
    /// resolves lazily.
    unknown,

    /// Exact byte length. The host reports this in stat and uses it
    /// for read termination.
    exact(u64),
}

record dir-entry {
    name: string,
    kind: entry-kind,
    size: entry-size,
    projected-files: option<list<projected-file>>,
}
```

`preloaded-entry.size` follows the same shape.

The `projected-file` record (used inside `dir-entry.projected-files`)
gains an explicit size field for the case where the provider has
already materialized the bytes:

```wit
record projected-file {
    name: string,
    content: list<u8>,
    // size is derived from content.len() at the host boundary; no
    // explicit field needed.
}
```

### Host FUSE layer

Three changes in `crates/host/src/fuse/`:

1. **Open flag.** When the inode's known size is `Unknown`, the
   `open` reply sets `FOPEN_DIRECT_IO`. When the size is `Exact`,
   open uses default flags (page cache enabled).
2. **Stat reporting.** `attr_for_kind` takes the projected size state
   directly. `Unknown` reports `st_size = 0`. `Exact(n)` reports
   `st_size = n`.
3. **Lazy size resolution.** After a read completes for an
   `Unknown`-sized file, the host updates the inode's cached size to
   the real bytes length (from the projection cache) and calls
   `fuse_lowlevel_notify_inval_inode(ino, 0, 0)` so the kernel
   re-fetches attrs on next access. The notification is best-effort;
   if it fails (kernel disconnect, fuser API gap), reads still work.

The notification step is the only nontrivial new machinery. fuser
exposes `Notifier::inval_inode` on recent versions; if the version
omnifs uses does not, the work item is to bump fuser or send a manual
FUSE_NOTIFY_INVAL_INODE message.

### Provider migrations

Per provider, the audit is mechanical:

- **arxiv**: `paper.pdf` and `source.tar.gz` use `Size::Unknown`.
  `metadata.json`, `links.json`, `authors.txt`, `comment.txt`,
  `selector.txt` use `file_with_content` (size derived from bytes).
- **github**: file content reads where the API returns
  `content.size` should pass `Size::Exact`. Releases assets
  (downloaded blobs) use `Size::Unknown` on the projection and rely
  on direct_io.
- **dns**: all current files are eagerly materialized; no change
  needed.
- **test**: irrelevant to size handling.

### macOS / macFUSE

macFUSE supports `direct_io` with the same semantics as Linux for
read pass-through and page cache bypass. mmap support on direct_io
files is not guaranteed across macFUSE versions; tools that mmap
projected files (which is rare for the providers in tree) may need
to fall back to read. This is documented; no code mitigation.

## Migration plan

1. Land WIT change for `entry-size` variant with a feature gate or
   versioned interface bump if any external consumer exists. (omnifs
   has none today, so a single atomic change is fine.)
2. Update host runtime to consume the new variant and to set
   `FOPEN_DIRECT_IO` for `Unknown` files.
3. Update SDK to expose `Size::{Exact, Unknown}` and rewrite the
   `Projection::file*` surface. Drop `DEFAULT_FILE_SIZE_BYTES` and
   `FileStat::placeholder`.
4. Update macros to accept `size = "exact" | "unknown"` and default
   to `unknown` for `#[file]`.
5. Sweep the three in-tree providers for the right size declaration
   per file.
6. Add a host-level test that opens an `Unknown` file, reads to the
   end, asserts a subsequent `stat` reports the real size.
7. Add a host-level test that opens an `Exact` file with mismatched
   real length and asserts the read is capped at the declared size
   (current behavior preserved for the `Exact` path).

Each step is independently reviewable. Steps 1-3 can land together;
4-5 follow; 6-7 close the loop.

## Open questions

- **Inode invalidation API**: confirm fuser version exposes
  `Notifier::inval_inode` (or equivalent). If not, decide between a
  fuser bump or a manual notify message.
- **Concurrent reads on the same `Unknown` file**: if two reads race
  and both complete, both will issue inode invalidation. Idempotent;
  no correctness issue, but worth verifying the kernel handles
  redundant invalidations gracefully (it does on Linux; verify on
  macFUSE).
- **Direct_io and FUSE writeback cache**: irrelevant today (omnifs is
  read-only for the file surface), but if mutations land via the
  filesystem path, the writeback story for direct_io files needs its
  own treatment.
- **Exact size lying**: if a provider declares `Size::Exact(n)` and
  the actual bytes differ, the host should detect at read time. A
  debug-build assertion is cheap; a release-build truncation matches
  current kernel-cap behavior. Decision: assert in debug, document
  the contract, no runtime check in release.
