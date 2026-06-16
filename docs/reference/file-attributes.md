---
title: "File attributes"
description: "Reference for projected file size, byte source, read mode, stability, version evidence, and the host policy derived from them."
---

Projected files declare facts about their bytes. The provider does not tell the host which FUSE flags to use or which cache to bypass. The host derives that policy from the declared attributes.

## Attribute set

| Attribute | Meaning |
|---|---|
| `Size` | What the provider knows about the byte length. |
| `byte-source` | Where bytes come from: inline bytes, canonical object bytes, blob bytes, or deferred reads. |
| `ReadMode` | Whether deferred bytes are read as a full payload or through ranged reads. |
| `Stability` | Whether bytes are immutable, mutable between observations, or volatile during observation. |
| `version-token` | Optional opaque evidence for one observed version. |

The current WIT names are `file-size`, `byte-source`, `read-mode`, `stability`, and `version-token`.

## Size

| Size | Meaning | Pre-read POSIX caveat |
|---|---|---|
| `exact(n)` | The provider knows the exact byte length. `exact(0)` is known empty. | Stat-only tools can rely on `st_size = n`. |
| `non-zero` | The provider knows the file is not empty, but not the exact length. | FUSE still needs a numeric `st_size`; the host uses a compatibility lower-bound until materialization. |
| `unknown` | The provider has no length information. | The host must avoid pretending the reported compatibility size is exact. |

There is no POSIX representation for unknown regular-file length. The design is intentionally honest about that boundary.

## Byte source

| Source | Use |
|---|---|
| `inline(bytes)` | Small bytes carried directly in the provider response. |
| `canonical` | Serve canonical object bytes stored in the object cache without copying them into the view cache. |
| `blob(blob-id)` | Serve large host-resident bytes from the blob cache. |
| `deferred(read-mode)` | Read bytes later through `read-file` or ranged file operations. |

Inline bytes are for small files. Larger payloads should use deferred, blob, or canonical sources so the provider does not push large byte vectors through every listing or lookup.

## Read mode

| Mode | Meaning |
|---|---|
| `full` | The provider can produce the whole payload for a read. |
| `ranged` | The provider can serve byte ranges through `open-file`, `read-chunk`, and `close-file`. |

Ranged reads are required for volatile files because the host cannot treat one eager byte vector as a stable snapshot of a moving source.

## Stability

| Stability | Meaning | Cache implication |
|---|---|---|
| `immutable` | Bytes for this file identity do not change. | Durable caching by identity is safe, subject to capacity and explicit invalidation. |
| `mutable` | Bytes may change between observations, but each observation should be consistent. | Durable reuse needs version evidence or invalidation proof. |
| `volatile` | Bytes may change while being observed. | Use live/ranged behavior; do not place whole-file bytes in durable caches. |

`mutable` is not an instruction to refetch on every open. It is a claim about the source. The host decides the cache policy from version evidence, invalidation coverage, and read shape.

## Structural rule

`volatile` files require deferred ranged bytes.

```text
Stability::Volatile => byte-source::deferred(read-mode::ranged)
```

This keeps live logs and other moving sources out of whole-file cache paths.

## Version evidence

A `version-token` can be an ETag, commit SHA, update timestamp, content digest, or other upstream revision identifier. It is opaque to the host and compared by byte equality.

Version tokens are useful for mutable files. They let the host treat repeated observations of the same version as the same snapshot. They do not make volatile files durable, and they are usually redundant for immutable identities.

## Tool compatibility

The file-attribute model exists because omnifs paths must work with normal shell tools:

- `cat`, `head`, `tail`, `less`, `file`
- `grep`, `rg`, `find`, `fd`
- `ls`, `du`, `wc`, `stat`
- `cp`, `tar`, `rsync`
- `diff`, `cmp`, `sha256sum`
- `jq`, `yq`, `xmllint`

Tools that make decisions only from `st_size` still need exact size to be exact. For example, `tar c`, `wc -c` fast paths, `tail -c`, and `find -size` cannot discover an unknown length unless the host has first materialized or learned it.
