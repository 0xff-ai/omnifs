---
title: Projections
description: Declaring projected files — Size, Bytes, ReadMode, Stability, version evidence — and how attributes map to host behavior.
---

A projection tells the host how to present a directory's children before a read, and how to fetch file bytes during one. You build a directory's contents with `Projection`, and return file bytes from a `#[file]` handler with `FileContent`. The underlying file metadata is a `FileProj` (attributes plus byte availability).

## The pieces of a file projection

A `FileProj` is file attributes plus byte availability:

- **Size** (`Size`): `Exact(u64)`, `NonZero`, or `Unknown`. Drives `st_size` in `stat`. `NonZero` and `Unknown` report `st_size = 1` until a real size is learned.
- **Bytes** (`ProjBytes`): `Inline(Vec<u8>)` — bytes available now — or `Deferred { read }` — bytes fetched later via the given read mode.
- **ReadMode** (`ReadMode`): `Full` (whole file via `read_file`) or `Ranged` (chunked). Only meaningful for deferred bytes.
- **Stability** (`Stability`): `Immutable` (never changes once seen), `Mutable` (may change between reads), `Volatile` (changes on every read).
- **Version evidence** (`Option<VersionToken>`): an opaque token the host uses to key durable, version-stamped content.

## Building a Projection

`Projection` is built mutably. Use the convenience methods to add children:

```rust
let mut p = Projection::new();

p.dir("subfolder");                       // a child directory

p.deferred_file("paper.pdf");             // deferred, Size::Unknown, ReadMode::Full, Immutable
p.file_with_stat("data.bin", FileStat::exact(4096));  // deferred, Size::Exact, Full, Immutable
p.file_with_content("meta.json", bytes);  // inline bytes, Immutable
p.file_with_content_attrs("rows.json", bytes, Stability::Mutable, version); // inline + attrs

p.file("custom.txt", FileProj::deferred(Size::NonZero, ReadMode::Full, Stability::Mutable));

p.page(PageStatus::Exhaustive);           // listing is authoritative
Ok(p)
```

For full control, construct a `FileProj` directly:

```rust
// Bytes you already hold. Size becomes Exact(bytes.len()) automatically.
FileProj::inline(content_bytes, Stability::Immutable, None)

// Bytes fetched on demand. You pick size, read mode, and stability.
FileProj::deferred(Size::NonZero, ReadMode::Full, Stability::Mutable)
    .with_version("etag:\"abc123\"")   // optional version token
```

## Returning file bytes from a `#[file]` handler

`FileContent` is the read terminal:

```rust
FileContent::bytes(vec_of_u8)                    // inline; default Immutable
FileContent::bytes_with_attrs(attrs, vec_of_u8)  // inline + explicit FileAttrs
FileContent::blob(blob_id)                        // serve from a host-resident blob
FileContent::blob_with_attrs(attrs, blob_id)      // blob + explicit FileAttrs
FileContent::range_bytes(attrs, vec_of_u8)        // large buffer served as a ranged read
```

`FileContent::blob(..)` keeps large bytes host-side: the blob never crosses the WIT boundary. `range_bytes` is the escape hatch the database provider uses when a sample exceeds the inline cap — it keeps the buffer and lets the host serve ranges from it.

## Byte source ↔ handler pairing

The byte declaration determines which handler the host calls on read. Pair them correctly:

| Projected as | Host read path | Handler you must provide |
| --- | --- | --- |
| inline (`file_with_content`, `FileProj::inline`) | served from the projection itself | none — bytes are already present |
| deferred Full (`deferred_file`, `file_with_stat`) | `read_file(path)` | a `#[file]` handler returning `FileContent` |
| deferred Ranged | `open_file` / `read_chunk` | a ranged read path (reserved; see below) |

A directory listing typically projects small files inline and large files deferred:

```rust
#[dir("/{category}/{paper_id}")]
async fn paper_dir(cx: &DirCx<State>, category: String, paper_id: String) -> Result<Projection> {
    let entry = api::fetch_entry(cx, &paper_id).await?;
    let mut p = Projection::new();
    p.file_with_content("metadata.json", entry.metadata_json_bytes(None)); // small -> inline
    p.deferred_file("paper.pdf");                                          // large -> deferred
    p.page(PageStatus::Exhaustive);
    Ok(p)
}
```

The matching `#[file]` handler supplies the deferred bytes when the file is read, serving a blob:

```rust
#[file("/{category}/{paper_id}/paper.pdf")]
async fn paper_pdf(cx: &Cx<State>, category: String, paper_id: String) -> Result<FileContent> {
    let blob = cx.http()
        .get(format!("https://arxiv.org/pdf/{paper_id}"))
        .into_blob()
        .with_cache_key(format!("arxiv-pdf-{paper_id}"))
        .send()
        .await?;
    Ok(FileContent::blob_with_attrs(
        FileAttrs::new(Size::Exact(blob.size), Stability::Immutable),
        blob.id(),
    ))
}
```

## Structural rules

The SDK validates projections; violations are recorded as a `ProviderError`:

- **Volatile requires Ranged.** A `Volatile` file must use `FileProj::deferred(_, ReadMode::Ranged, _)`. Volatile content changes on every read, so the host must re-fetch ranges rather than cache a snapshot.
- **Inline bytes require `Size::Exact(len)`** matching the actual byte length, and stay under the eager-byte cap (`MAX_PROJECTED_BYTES`, 64 KiB). `FileProj::inline` and `file_with_content` set the size for you; if a buffer might exceed the cap, switch to `FileContent::range_bytes`.

```rust
// Rejected: volatile content cannot be inline or full.
FileProj::deferred(Size::Unknown, ReadMode::Full, Stability::Volatile); // invalid

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
The ranged read path (`open_file` / `read_chunk` / `close_file`) is reserved in the WIT. The current host/runtime path serves exact file bytes via `read_file`, plus `FileContent::range_bytes` for an in-memory buffer, plus subtree handoff. Prefer `deferred_file` + a `#[file]` handler, or a blob handoff, unless you are specifically building against the ranged path.
:::
