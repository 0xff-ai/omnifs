---
title: Read and inspect
description: Read projected files and fields with standard tools.
---

# Read and inspect

**Read a field as a file**

    cat /omnifs/github/rust-lang/rust/issues/open/12345/title

The issue is fetched once, so reading `body` or `state` next door does not hit the network. The provider projected the whole issue from one payload.

**Page a long body**

    less /omnifs/github/rust-lang/rust/issues/open/12345/body

Ordinary `less`, with no wrapper. The first read is cold, and later reads come from cache.

**Check what a file is before reading it**

    file /omnifs/arxiv/papers/2301.00001/v1/pdf

Content types are accurate, so `file`, editors, and viewers all behave correctly.

**Tail a long sample**

    tail -n 5 /omnifs/db/tables/artists/sample.json

The first read of a path may call upstream, and the next is served from cache.
