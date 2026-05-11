# Projected file sizes: honest stat, direct_io, and lazy resolution

Status: proposal from `design/projected-file-sizes`; not implemented in this branch
Scope: `wit/provider.wit`, host FUSE layer + cache schema, SDK projection API, providers
Branch: design/projected-file-sizes

Current implementation note: this branch still uses `FileStat`,
`DEFAULT_FILE_SIZE_BYTES`, WIT `option<u64>` sizes, and cache schema
version 2. The sections below describe the target design from the
projected-file-sizes branch, not the implementation in
`sandbox-archive-hardening`.

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
- Linux behavior is the implementation target for the current repo. The
  original proposal also considered macFUSE, but macOS support is out of
  scope here.

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

`Size` lives in `crates/omnifs-sdk/src/browse.rs` (re-exported through
the prelude):

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Size {
    Unknown,
    Exact(NonZeroU64),
}

impl Size {
    pub fn from_content_len(len: usize) -> Self { ... }
}
```

`Projection` exposes three ways to declare a file:

- `projection.file(name)` — size unknown until read (the common case
  for upstream blobs like PDFs and tarballs).
- `projection.file_with_size(name, Size::Exact(n))` — size known up
  front, no bytes inline (used when an API returns a byte count
  alongside metadata).
- `projection.file_with_content(name, bytes)` — projection
  materializes the payload inline; size is derived from `bytes.len()`
  and used directly.

The target design removes the placeholder constant
`DEFAULT_FILE_SIZE_BYTES`, `FileStat::placeholder`, and `FileStat`
entirely.
Static-shape entries auto-derived from `#[file]` declarations report
`Size::Unknown` so the host opens them with direct_io until the read
resolves the real length.

No macro change. The earlier sketch added `size = "exact" | "unknown"`
to `#[file]`, but the macro fires at registration time and cannot see
the eventual byte length, so the argument would have been
load-bearing only as a default for the auto-derived static shape.
Always picking `Unknown` for that case is simpler and equally honest.

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

`projected-file` is unchanged: when the provider materializes bytes
inline, the host derives the size from `content.len()` at the WIT
boundary; no explicit field is needed.

### Host FUSE layer target

The target design needs three changes in `crates/host/src/fuse/` and
`crates/host/src/runtime/`:

1. **Open flag.** `FuseFs::open` sets `FopenFlags::FOPEN_DIRECT_IO`
   when the inode's `size` is `None`. When it is `Some(n)`, open uses
   default flags (page cache enabled).
2. **Stat reporting.** `attr_for_kind` takes `Option<u64>` directly.
   `None` reports `st_size = 0`. `Some(n)` reports `st_size = n`.
3. **Lazy size resolution.** After `OpResult::Read` returns content,
   `FuseFs::resolve_unknown_size` updates the inode's cached size
   (when previously `None`) and calls
   `CalloutRuntime::notify_inval_inode`, which forwards to
   `fuser::Notifier::inval_inode(ino, 0, 0)`. fuser 0.17 exposes the
   notifier method directly; no version bump needed. Best-effort: if
   the notifier is gone (mount tearing down), reads still succeed.

The cache schema would bump from version 2 to version 3 because
`LookupPayload`, `AttrPayload`, and `DirentRecord` now serialize
`size: Option<u64>` instead of a `u64` sentinel. The host's
`From<wit_types::EntrySize> for Option<u64>` impl bridges WIT to
cache types.

### Provider migration target

In the target design, `Projection::file(name)` keeps its meaning
("declare a file with no inline content") and reports `Size::Unknown`.
Existing call sites compile unchanged; the host opens those files with
direct_io. Providers can opt into `file_with_size(name, Size::Exact(n))`
when an upstream API hands them a byte count cheaply (for example
github's `content.size` for blob reads).

### macOS / macFUSE note

The current repository is Linux-only. The original proposal considered
macFUSE behavior, but that is not an implementation requirement for this
branch.

## Open questions and follow-ups

- **Concurrent reads on the same `Unknown` file**: if two reads race
  and both complete, both call `notify_inval_inode`. Idempotent on
  Linux.
- **Direct_io and FUSE writeback cache**: irrelevant today (omnifs is
  read-only for the file surface). If mutations land via the
  filesystem path, the writeback story for direct_io files needs its
  own treatment.
- **Exact size lying**: if a provider declares `Size::Exact(n)` and
  the actual bytes differ, the kernel caps the read at `n` (current
  behavior). A debug-build assertion would catch this in tests; not
  added yet.
- **Coverage tests**: the integration suite exercises the unknown →
  read → updated-size path end-to-end through the existing
  `runtime_test.rs` cases. A targeted test that asserts
  `notify_inval_inode` fires (and that a subsequent stat sees the
  real size) would close the loop on the lazy-resolution leg
  specifically; currently that depends on FUSE wiring that is hard
  to exercise outside an actual mount.
