---
title: Search and traverse
description: Grep, find, and walk the projected tree like any local directory.
---

# Search and traverse

**Grep across every open issue**

    grep -rl "segfault" /omnifs/github/rust-lang/rust/issues/open

One grep does it, with no API client and no pagination loop. The first sweep warms the cache, and later runs read at SSD speed.

**Find by name**

    find /omnifs/db/tables -name 'schema.sql'

Plain `find` works because the tree behaves like a real directory hierarchy.

A note on listings. `lookup` is the authoritative name oracle, and a directory listing can be non-exhaustive. A specific path may resolve even when its name did not appear in the latest `ls`, which is how a provider offers an effectively infinite namespace without enumerating it. If you know the exact path, read it, and do not assume `ls` showed every name.
