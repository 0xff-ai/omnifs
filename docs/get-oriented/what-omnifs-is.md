---
title: What omnifs is
description: omnifs mounts external systems as local filesystem paths so any tool can read them without a service-specific client.
---

# What omnifs is

omnifs mounts external systems as local filesystem paths. A GitHub repository, a Linear workspace, a SQLite database, a DNS zone: each becomes directories and files you can `cd` into, `cat`, `grep`, and pipe through any tool you already have. You address a resource by where it lives, not through an API client you have to write.

## The spine

One line carries the idea: open a path, read the world. Behind every path is a provider, a sandboxed component that turns one external system into files. You mount a provider, and its slice of the world appears under `/omnifs`.

## The four primitives

omnifs is built from four ideas, and everything else follows from them.

Projection, not sync. omnifs does not copy a service ahead of time. It materializes exactly the slice you touch, when you touch it. You cannot copy the whole world, but you can address the part of it you reach.

A Unix-native surface. The projected tree behaves like real files, so the tools built on the filesystem over the last fifty years work on it unchanged: `grep`, `find`, `make`, `rsync`, your editor, your shell scripts, your agent.

Sandboxed providers. Each provider runs as a `wasm32-wasip2` WebAssembly component with no ambient authority. It has no access to the network, the filesystem, or your credentials on its own. Because of that, you can mount a provider you did not write.

Host-owned trust. The host owns credentials, network calls, and the cache. Providers decide what paths mean. The host decides what actually happens. That split is the core of the design.

## Who reads the tree

The mount is process-universal: it serves every program on the machine at once, and none of them needs to know omnifs exists. A human greps it from a shell. A script pipes it into `make`. An agent reads a path instead of loading an API schema into its context. An application points at a path and inherits the integration without writing one. The same tree serves all of them, from the same paths.
