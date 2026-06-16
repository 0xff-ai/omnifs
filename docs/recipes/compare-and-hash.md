---
title: Compare and hash
description: Diff and hash projected files with standard tools.
---

# Compare and hash

**Diff two resources**

    diff /omnifs/github/rust-lang/rust/issues/open/100/state \
         /omnifs/github/rust-lang/rust/issues/open/200/state

Plain `diff` works over two paths, and each side is a real file the moment you read it.

**Hash a projected file**

    sha256sum /omnifs/arxiv/papers/2301.00001/v1/pdf

An immutable resource like a specific paper version hashes the same on every read, because it caches by identity.

Diffing two dated snapshots of a provider tree is on the roadmap: it needs the snapshot feature, not just the cache. Today `diff` works between any two live paths.
