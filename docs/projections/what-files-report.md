---
title: What files report
description: What projected files honestly tell tools about size, freshness, content type, and read behavior — and why those facts matter to the toolbox.
---

When you run `ls -l` or `stat` on an omnifs path, you get numbers. When `wc -c` fast-paths on a file size, or `tail -c` seeks to the end, or `find -size` filters, those commands make decisions from what the filesystem reports. omnifs takes that seriously. Projected files declare what they know honestly, not optimistically.

## Size: exact, non-zero, or unknown

A projected file reports one of three things about its byte length.

If the provider knows the exact length, `st_size` is that length. Tools that stat-only can rely on it.

If the provider knows the file is non-empty but not its exact length, the host must report some non-negative integer in `st_size` anyway (POSIX requires it). The reported number is a compatibility lower-bound, not a promise. Tools that read bytes directly will get the real content; tools that make decisions from `st_size` alone may be wrong before the file is materialized.

If the provider has no length information, `st_size` is also a lower-bound placeholder. There is no POSIX representation for "unknown regular-file length." The design names this boundary rather than pretending the placeholder is accurate.

This honesty matters for tools like `tar c`, `wc -c` fast paths, `tail -c`, and `find -size`. They all depend on `st_size` being exact. If a file's exact size is not known until it is read, those tools discover that on first materialization.

## Freshness: immutable, mutable, or volatile

Projected files are not all the same kind of thing. A specific commit SHA in a Git repository is immutable: the bytes for that identity will not change. A GitHub issue title is mutable: it could be edited upstream. A live log tail is volatile: bytes may change while you are reading them.

Those distinctions change what the host can safely cache. An immutable file can be held durably by identity. A mutable file needs version evidence or invalidation proof before the host reuses a cached copy. A volatile file must not be placed in a whole-file durable cache at all — the host instead keeps it in a live, ranged read path.

From a tool's perspective, mutable and immutable files look the same in a fresh read. The difference shows up in whether a second read can return the same bytes from cache or must go back to the upstream source.

## Read behavior: full payload vs ranged reads

Most files read as a full payload: the provider returns all the bytes in one response. Live logs and other volatile sources use ranged reads instead, which lets the host serve `tail -f` and byte-range requests without treating a snapshot as stable.

The distinction is declared, not inferred. A volatile file is required to use ranged reads. Any other file can declare either mode, and the provider authoring guide covers when ranged makes sense beyond the volatile case.

## Why the toolbox gets this right

omnifs paths must work with normal shell tools. The file-attribute model exists so that `cat`, `head`, `tail -f`, `grep -r`, `rg`, `find -name`, `find -size`, `ls -l`, `du -sh`, `wc -l`, `wc -c`, `stat`, `cp`, `tar`, `rsync`, `diff`, `cmp`, `sha256sum`, `jq`, `yq`, and `xmllint` all behave as expected. Each attribute maps to a real FUSE or cache policy decision. Nothing here is aspirational.

The full attribute set, including enum names and the structural rule that volatile files require deferred ranged bytes, lives in the file-attributes reference page.
