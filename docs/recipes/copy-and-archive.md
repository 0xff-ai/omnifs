---
title: Copy and archive
description: Copy projected files to disk and archive subtrees with cp, tar, and rsync.
---

# Copy and archive

**Copy a projected file to disk**

    cp /omnifs/arxiv/papers/2301.00001/v1/pdf ~/paper.pdf

A large binary is served through the blob cache, so the copy streams from the host rather than re-downloading.

**Archive a subtree**

    tar czf issues.tgz -C /omnifs/github/rust-lang/rust/issues open

`tar` reads the projected tree like any directory. Reading materializes exactly what you touch, and nothing is fetched ahead of need.

**Mirror with rsync**

    rsync -a /omnifs/db/tables/ ./tables-snapshot/

This pulls the current projection to local disk. What you copy is whatever was warm or freshly fetched at copy time.
