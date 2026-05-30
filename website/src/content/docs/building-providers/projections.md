---
title: Projections
description: Declaring projected files — Size, Bytes, ReadMode, Stability, version evidence — and how attributes map to host behavior.
---

A projected file tells the host how to present a file before, and how to fetch it during, a read. You build projections with `FileProj` and attach them to directory entries via `Entry::file`, or return file content directly from a `#[file]` handler with `FileContent`.

## The pieces of a projection

A `FileProj` is file attributes plus byte availability:

- **Size** (`Size`): `Exact(u64)`, `NonZero`, or `Unknown`. Drives the file's `st_size` in `stat`.
- **Bytes** (`ProjBytes`): either `Inline(Vec<u8>)` — bytes available now — or `Deferred { read }` — bytes fetched later via the given read mode.
- **ReadMode** (`ReadMode`): `Full` (whole file via `read_file`) or `Ranged` (chunked via `open_file`/`read_chunk`). Only meaningful for deferred bytes.
- **Stability** (`Stability`): `Immutable` (never changes once seen), `Mutable` (may change between reads), `Volatile` (changes on every read).
- **Version evidence** (`Option<VersionToken>`): an opaque token the host uses to key durable, version-stamped content.

## Constructing projections

The SDK gives you three constructors covering the common shapes:

```rust
// Bytes you already have. Host serves them directly; no read callout.
FileProj::inline(content_bytes, Stability::Immutable, None)

// Bytes fetched on demand, whole-file. Host calls read_file when opened.
FileProj::deferred_full(Size::NonZero, Stability::Mutable, None)

// Bytes fetched on demand, ranged. Host calls open_file/read_chunk.
FileProj::deferred_ranged(Size::Unknown, Stability::Volatile, None)
```

Add a version token when you have one (an ETag, a commit sha, a content hash):

```rust
let attrs = FileAttrs::new(Size::Exact(len), Stability::Mutable)
    .with_version("etag:\"abc123\"");
```

## Byte source ↔ handler pairing

The projection's byte declaration determines which handler the host calls on read. Pair them correctly or the read fails:

| Projection bytes | Host read path | Handler you must provide |
| --- | --- | --- |
| `inline(..)` | served from the projection itself | none — bytes are already present |
| `deferred_full(..)` | `read_file(path)` | a `#[file]` handler returning `FileContent` |
| `deferred_ranged(..)` | `open_file` / `read_chunk` | a ranged read handler (reserved; see below) |

A directory listing typically projects inline content for small files and deferred content for large ones:

```rust
#[dir("{category}/{paper_id}")]
fn paper_dir(category: &str, paper_id: &str, cx: &Cx) -> Result<List> {
    let entry = first_entry(cx, paper_id)?;
    let files = vec![
        // small, already fetched -> inline
        Entry::file("abstract.txt", FileProj::inline(entry.summary.clone().into_bytes(), Stability::Immutable, None)),
        // large, fetched on read -> deferred full
        Entry::file("paper.pdf", FileProj::deferred_full(Size::NonZero, Stability::Immutable, None)),
    ];
    Ok(List::entries(Listing::complete(files)))
}
```

The matching `#[file]` handler supplies the deferred bytes when the file is read:

```rust
#[file("{category}/{paper_id}/paper.pdf")]
fn paper_pdf(category: &str, paper_id: &str, cx: &Cx) -> Result<FileContent> {
    let blob = cx.fetch_blob(Request::get(format!("https://arxiv.org/pdf/{paper_id}")),
                             format!("arxiv-pdf-{paper_id}"))?;
    Ok(FileContent::blob(blob.id()))
}
```

`FileContent::blob(..)` keeps large bytes host-side: the blob never crosses the WIT boundary. `FileContent::new(bytes)` returns inline bytes directly.

## Structural rules

The SDK validates projections; violations return a `ProviderError`:

- **Volatile requires Ranged.** A `Volatile` file must use `deferred_ranged`. Volatile content changes on every read, so the host must re-fetch ranges rather than cache a snapshot. `inline` or `deferred_full` with `Volatile` is rejected.

```rust
// Rejected: volatile content cannot be inline or full.
FileProj::inline(bytes, Stability::Volatile, None);        // error
FileProj::deferred_full(Size::Unknown, Stability::Volatile, None); // error

// Correct: volatile -> ranged.
FileProj::deferred_ranged(Size::Unknown, Stability::Volatile, None);
```

## How attributes map to host behavior

- **Size** becomes `st_size`. `Exact` gives a precise size for `ls -l`, `du`, `wc -c`. `NonZero` and `Unknown` let the host learn and promote a real size after the first read.
- **Stability** controls cache lifetime and whether the host may serve a cached snapshot. `Immutable` content is cached indefinitely until invalidated; `Mutable` may be re-fetched; `Volatile` is never snapshot-cached.
- **ReadMode** controls FUSE direct I/O and whether the host opens a ranged handle.
- **Version token** lets the host store durable, version-keyed content and skip refetches when the token is unchanged.

:::note
The full design — every legal combination, learned-size promotion, and post-read invalidation — is in `docs/design/file-attributes.md`. Read it before adding a new `#[file]` handler shape or extending the projection API.
:::

:::caution
Ranged reads (`open_file` / `read_chunk` / `close_file`) are reserved in the WIT. The current host/runtime path serves exact file bytes via `read_file` and explicit subtree handoff. Prefer `deferred_full` plus a `#[file]` handler, or a blob handoff, unless you are specifically building against the ranged path.
:::
