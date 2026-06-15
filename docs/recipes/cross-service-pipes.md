---
title: Cross-service pipes
description: Grep and join across multiple providers in a single shell command.
---

# Cross-service pipes

**Find an incident across two trackers**

    grep -rl "ACME outage" /omnifs/github/rust-lang/rust/issues/open /omnifs/linear/teams/ENG/issues/open

One grep searches two services at once, with no SDK and no tokens in your script. Each hit is a real path you can `cat`.

**Join state across providers**

    for n in $(jq -r '.[].number' /omnifs/github/.../issues.json); do
      cat /omnifs/github/rust-lang/rust/issues/open/$n/title
    done

There is no integration to write. N providers serve every consumer, and here the consumer is plain shell. The same join is a multi-SDK project anywhere else.

The first sweep over a corpus warms the cache, and repeat runs are local. If a mount comes back empty, check `omnifs auth status` before assuming there is nothing there.
