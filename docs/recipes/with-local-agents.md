---
title: Use omnifs with local agents
description: Give a local agent access to projected paths without writing any integration code.
---

# Use omnifs with local agents

**Mount under your agent**

You do not integrate omnifs with an agent. You mount it under the agent. Point the agent's shell at `/omnifs` and it reads the world with the `cat`, `ls`, and `grep` it already knows.

**Let the tree teach itself**

    ls /omnifs/github
    cat /omnifs/github/README

A provider's self-describing directories and schema leaves mean an agent's first contact is productive within one `ls`, instead of loading an API schema into context.

**Read a path instead of carrying a schema**

    cat /omnifs/linear/teams/ENG/issues/open

Reaching a path costs the agent the path, not a re-described service in its context window. State this as less context to hand the agent, not as a benchmarked token figure. The measured number ships with the benchmark, not before.

Read-only is the safety property here: an agent cannot break what it cannot write, and a human can `cat` exactly what the agent read, at the same path.
