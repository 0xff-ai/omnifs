---
title: Projections
description: Declaring projected files — Size, Bytes, ReadMode, Stability, version evidence — and how attributes map to host behavior.
---

A projected file tells the host how to present a file before a read, and how to fetch it during one. You build the metadata with `FileProj` and attach it to directory entries via `Entry::file(name, proj)`. You return actual bytes from a `#[file]` handler with `FileContent`.

## The pieces of a projection

A `FileProj` is file attributes plus byte availability:

- **Size** (`Size`): `Exact(u64)`, `NonZero`, or `Unknown`. Drives `st_size` in `stat`. `NonZero` and `Unknown` report `st_size = 1` until a real size is learned.
- **Bytes** (`ProjBytes`): `Inline(Vec<u8>)` — bytes available now — or `Deferred { read }` — bytes fetched later via the given read mode.
- **ReadMode** (`ReadMode`): `Full` (whole file via `read_file`) or `Ranged` (chunked via `open_file`/`read_chunk`). Only meaningful for deferred bytes.
- **Stability** (`Stability`): `Immutable` (never changes once seen), `Mutable` (may change between reads), `Volatile` (changes on every read).
- **Version evidence** (`Option<VersionToken>`): an opaque token the host uses to key durable, version-stamped content.

## Constructing projections

Two constructors cover the common shapes:

```rust
// Bytes you already hold. Host serves them directly; no read callout.
// Size is set to Exact(bytes.len()) automatically.
FileProj::inline(content_bytes, Stability::Immutable, None)

// Bytes fetched on demand. You pick size, read mode, and stability.
FileProj::deferred(Size::NonZero, ReadMode::Full, Stability::Mutable)
```

Attach a version token when you have one (an ETag, a commit sha, a content hash):

```rust
FileProj::deferred(Size::Unknown, ReadMode::Full, Stability::Mutable)
    .with_version("etag:\"abc123\"")
```

## Byte source ↔ handler pairing

The projection's byte declaration determines which handler the host calls on read. Pair them correctly:

| Projection bytes | Host read path | Handler you must provide |
| --- | --- | --- |
| `inline(..)` | served from the projection itself | none — bytes are already present |
| `deferred(_, ReadMode::Full, _)` | `read_file(path)` | a `#[file]` handler returning `FileContent` |
| `deferred(_, ReadMode::Ranged, _)` | `open_file` / `read_chunk` | a ranged read handler (reserved; see below) |

A directory listing typically projects small files inline and large files deferred:

```rust
#[dir("/{category}/{paper_id}")]
async fn paper_dir(cx: Cx<State>, category: String, paper_id: String) -> Result<Listing> {
    let entry = api::fetch_entry(&cx, &paper_id).await?;
    Ok(Listing::complete(vec![
        // small, already fetched -> inline
        Entry::file("abstract.txt", FileProj::inline(entry.summary.into_bytes(), Stability::Immutable, None)),
        // large, fetched on read -> deferred full
        Entry::file("paper.pdf", FileProj::deferred(Size::NonZero, ReadMode::Full, Stability::Immutable)),
    ]))
}
```

The matching `#[file]` handler supplies the deferred bytes when the file is read:

```rust
#[file("/{category}/{paper_id}/paper.pdf")]
async fn paper_pdf(cx: Cx<State>, category: String, paper_id: String) -> Result<FileContent> {
    let blob = cx.http()
        .get(format!("https://arxiv.org/pdf/{paper_id}"))
        .into_blob()
        .with_cache_key(format!("arxiv-pdf-{paper_id}"))
        .send()
        .await?;
    Ok(FileContent::blob(blob.id()))
}
```

`FileContent::blob(..)` keeps large bytes host-side: the blob never crosses the WIT boundary. `FileContent::new(bytes)` returns inline bytes directly and defaults to `Size::Exact`, `Stability::Immutable`; override with `.with_attrs(..)`.

## Structural rules

The SDK validates projections; violations return a `ProviderError`:

- **Volatile requires Ranged.** A `Volatile` file must use `deferred(_, ReadMode::Ranged, ..)`. Volatile content changes on every read, so the host must re-fetch ranges rather than cache a snapshot.
- **Inline bytes require `Size::Exact(len)`** matching the actual byte length, and stay under the eager-byte cap (`MAX_PROJECTED_BYTES`, 64 KiB). Use `FileProj::inline`, which sets the size for you.

```rust
// Rejected: volatile content cannot be inline or full.
FileProj::deferred(Size::Unknown, ReadMode::Full, Stability::Volatile); // error at validate

// Correct: volatile -> ranged.
FileProj::deferred(Size::Unknown, ReadMode::Ranged, Stability::Volatile);
```

## How attributes map to host behavior

- **Size** becomes `st_size`. `Exact` gives a precise size for `ls -l`, `du`, `wc -c`. `NonZero`/`Unknown` let the host learn and promote a real size after the first read.
- **Stability** controls cache lifetime: `Immutable` is held until invalidated, `Mutable` may be re-fetched, `Volatile` is never snapshot-cached.
- **ReadMode** controls FUSE direct I/O and whether the host opens a ranged handle.
- **Version token** lets the host store durable, version-keyed content and skip refetches when the token is unchanged.

:::note
The full design — every legal combination, learned-size promotion, and post-read invalidation — is in `docs/design/file-attributes.md`. Read it before adding a new `#[file]` handler shape or extending the projection API.
:::

:::caution
Ranged reads (`open_file` / `read_chunk` / `close_file`) are reserved in the WIT. The current host/runtime path serves exact file bytes via `read_file` and explicit subtree handoff. Prefer `deferred(.., ReadMode::Full, ..)` plus a `#[file]` handler, or a blob handoff, unless you are specifically building against the ranged path.
:::
