# File attributes for projected files

Status: design / proposed
Scope: `wit/provider.wit`, host FUSE layer, SDK projection API, providers
Supersedes the relevant parts of: `docs/design/projected-file-sizes.md` (the size-only model)

## Problem

A projected file in omnifs needs to communicate three independent properties to the host: how large it is (or claims to be), how reads reach the bytes, and whether the bytes can change. Earlier the design tried to compress these into:

- a single `option<u64>` size with a 256 MiB placeholder for "unknown,"
- an implicit "lazy" read protocol (the host always called `read-file`),
- a `volatile: bool` flag that bundled cache-bypass + DIRECT_IO + zero TTLs + session-routed reads + fh-aware getattr into one bit.

Each of those compressions caused real bugs:

- `tail -n` panicked on the 256 MiB placeholder.
- `tar c` archived 256 MiB worth of pad bytes for unsized files.
- Volatile bundled four distinct concerns into one toggle, so streamed-but-stable files (e.g. a 200 MiB image layer that should be chunked but cached) had no expressible shape.

The redesign treats each property as its own attribute on the dir-entry — the same conceptual category as `size`, `mtime`, `mode` — and lets the host wire FUSE flags, cache layers, and post-read invalidation by reading them.

## Bash-tool compatibility (the durable invariant)

omnifs paths must behave like real files for the standard Linux toolbox. The "file-attribute model" exists to make this true. Tools the design must not regress:

- **Read content**: `cat`, `head`, `tail` (incl. `-f`, `-n`, `-c`), `less`, `more`, `xxd`, `hexdump`, `od`, `file`.
- **Search and traversal**: `grep` (incl. `-r`), `rg`, `find` (incl. `-name`, `-size`, `-type`), `fd`.
- **Stat-based**: `ls` (incl. `-l`, `-h`), `du` (incl. `-sh`), `wc` (incl. `-l`, `-c`, `-m`), `stat`.
- **Copy and archive**: `cp`, `mv`, `tar` (`c`, `x`, `t`), `rsync`.
- **Compare and hash**: `diff`, `cmp`, `md5sum`, `sha256sum`, `b3sum`.
- **Inspection**: `jq`, `yq`, `xmllint`.
- **Editors**: `vim`, `neovim`, `nano`. Editors that mmap (some `code` configurations) are best-effort but should not break.

The deeper invariant the tool list is a witness for: **metadata is truthful or explicitly unavailable; bytes are coherent for the declared mutability scope**. Tools that consult metadata before reading must not be misled into wrong decisions; tools that read must see consistent bytes for the duration the metadata promised.

## Attributes

```rust
pub struct FileAttrs {
    pub size: Size,
    pub access: Access,
    pub mutability: Mutability,
}

pub enum Size {
    /// Known precisely. `Exact(0)` is "known empty".
    Exact(u64),
    /// Provider's best non-zero guess. The host reports it as `st_size`
    /// so stat-only tools (`find -size`, `tar c`, `rsync --size-only`,
    /// file-manager UIs) see the file as non-empty and proceed to
    /// read it. NOT trusted for read clamping; FOPEN_DIRECT_IO ensures
    /// the kernel passes reads through verbatim. Post-read,
    /// `notify_inval_inode` updates `st_size` to the real length.
    Fuzzy(NonZeroU64),
    /// Provider has no information. Host substitutes a symbolic non-zero
    /// default, sets FOPEN_DIRECT_IO, and resolves to the real length
    /// post-read.
    Unknown,
}

pub enum Access {
    /// Bytes shipped with the projection. Host serves all reads from
    /// the cached payload; the provider isn't re-invoked.
    Inline,
    /// Host fetches all bytes via one `read-file` call when first
    /// needed and slices in memory. Provider returns the whole file.
    Full,
    /// Host opens a per-fh session and asks the provider for arbitrary
    /// byte ranges via `read-chunk`. Required for files where holding
    /// the whole payload in RAM is wasteful, and for any file whose
    /// bytes evolve during one open.
    Ranged,
}

pub enum Mutability {
    /// Bytes never change. Cache indefinitely; FUSE TTL is effectively
    /// forever.
    Immutable,
    /// Bytes consistent within one FUSE open; may differ across opens.
    /// Each `cat` sees one coherent state; a later `cat` may see a
    /// different one.
    Mutable,
    /// Bytes may differ between reads within one open. Host bypasses
    /// L0/L2 + per-fh cache; FUSE TTL = 0; FOPEN_DIRECT_IO; getattr/
    /// lseek fh-aware.
    Volatile,
}
```

### Why `Fuzzy` exists

Reporting `st_size = 0` for unknown-size files is honest but breaks stat-only tools: `find -size +0`, `tar c`, `rsync --size-only`, file managers skip empty files or treat them specially. For a projected fs we WANT those tools to read the file (that's how bytes get materialized), so a non-zero stat is load-bearing. `Fuzzy` is a deliberate, bounded lie — replaced by the real length after the first read via `notify_inval_inode`. Never trusted for read clamping; `FOPEN_DIRECT_IO` ensures the kernel always passes reads through to FUSE.

### Why three Mutability states, not two

Each maps to a distinct host wiring:

- `Immutable` caches forever; FUSE TTL is effectively infinite.
- `Mutable` caches per-open and refreshes on next open; standard FUSE TTL flow with kernel-managed page cache.
- `Volatile` bypasses every cache and runs the OFS-41 session machinery: cache-bypass, FOPEN_DIRECT_IO, zero TTLs, fh-aware getattr/lseek.

Collapsing `Mutable` and `Volatile` into one would force the host to choose between paying the volatility tax for every mutable file (massive perf regression for `inspect.json` and friends) or serving stale bytes from page cache for live logs. The three-way split is structural, not cosmetic.

## Validation: legal combinations

One structural rule constrains the combinations:

- **`Mutability::Volatile` requires `Access::Ranged`.** A non-Ranged Volatile file would Frankenstein sequential reads from different fetches; range reads via session are the only way to keep bytes coherent within one open while still allowing them to evolve.

`Inline` is compatible with both `Immutable` and `Mutable` — each lookup yields a fresh projection, so the inline bytes for a path can differ across listings while staying stable within one open. Only `Inline + Volatile` is excluded (and that exclusion follows from the rule above, since Volatile requires Ranged and Inline is not Ranged).

The SDK enforces the rule at registry build time. Nine `(Access, Mutability)` pairs become seven valid combinations:

| Access | Mutability | Used for |
|---|---|---|
| `Inline` | `Immutable` | static text shipped with the projection (e.g. `_README.md`) |
| `Inline` | `Mutable` | small payload re-fetched at projection time and shipped inline (e.g. `state` when small enough to inline) |
| `Full` | `Immutable` | small static file fetched on first read |
| `Full` | `Mutable` | one-shot fetch per open; the default for most lazy files |
| `Ranged` | `Immutable` | large stable file (e.g. image layer) where chunked reads matter |
| `Ranged` | `Mutable` | large file whose content may differ across opens |
| `Ranged` | `Volatile` | logs, stats, top — bytes evolve during one open |

## Declaring files

The provider declares files via `Projection::file(name, attrs)`. Bytes for `Inline` files travel in a separate `inline_bytes` field on the entry; bytes for `Full`/`Ranged` files come from a matching `#[file("/path")]` handler.

```rust
p.dir("by-name");

p.file("_README.md", FileAttrs {
    size: Size::Exact(README.len() as u64),
    access: Access::Inline,
    mutability: Mutability::Immutable,
});
p.set_inline_bytes("_README.md", README.into());

p.file("inspect.json", FileAttrs {
    size: Size::Unknown,
    access: Access::Full,
    mutability: Mutability::Mutable,
});

p.file("state", FileAttrs {
    size: Size::Exact(8),
    access: Access::Full,
    mutability: Mutability::Mutable,
});

p.file("layer.tar", FileAttrs {
    size: Size::Exact(layer_size),
    access: Access::Ranged,
    mutability: Mutability::Immutable,
});

p.file("logs/tail", FileAttrs {
    size: Size::Fuzzy(NonZeroU64::new(16 * MIB).unwrap()),
    access: Access::Ranged,
    mutability: Mutability::Volatile,
});
```

For the byte-producing handlers:

| `Access` | Bytes from | Handler returns |
|---|---|---|
| `Inline` | `Projection::set_inline_bytes(name, bytes)` | (no handler needed) |
| `Full` | matched `#[file]` handler | `FileContent::Bytes(...)` |
| `Ranged` | matched `#[file]` handler | `FileContent::Session(...)` |

The SDK validates the attrs ↔ handler pairing at registry build time: a `Full`-declared path whose handler returns `Session` (or vice versa) is rejected before the provider serves any requests. `Inline`-declared paths must not have a `#[file]` handler.

## Why the 256 MiB placeholder is gone

Earlier versions of omnifs reported a 256 MiB placeholder for unsized files. That single value tried to be both a read-termination upper bound (must be larger than any payload to avoid truncation) and a userspace size report (must be small enough to not lie). 256 MiB was the compromise; it was unsound in both directions. `tail -n` would `lseek(SEEK_END)` to 256 MiB and panic walking back through unallocated space; `du -sh` over a directory of projected files reported terabytes; some real downloads exceeded the bound and silently truncated.

The current model decouples the two concerns. `st_size` reports either a true size (`Exact`), a non-zero approximation (`Fuzzy`), or a small symbolic default the host substitutes for `Unknown`. `FOPEN_DIRECT_IO` is set on every open whose declared size isn't `Exact`, so the kernel passes reads through verbatim regardless of what stat said. After the first successful read, `notify_inval_inode` fires and the next stat reports the real size.

See `docs/design/projected-file-sizes.md` for the original size-only design doc.
