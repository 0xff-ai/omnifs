---
title: Stat and measure
description: List sizes, count rows, and inspect file metadata on projected paths.
---

# Stat and measure

**List with sizes**

    ls -l /omnifs/db/tables/artists

File sizes are accurate where the provider knows them. Where it does not, you get a sentinel value instead of the exact byte count.

**Count rows**

    cat /omnifs/db/tables/artists/count.txt

The count is its own leaf, so you read it without materializing the rows.

A note on learned sizes. For deferred content the exact size may be unknown until the first full read. Afterwards the host publishes the real size, so `ls -l` before and after a read can differ for the same file. That is expected, not a bug.
