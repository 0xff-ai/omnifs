---
title: Provider catalogue
description: The providers that ship with omnifs today, the paths they project, and where to read more.
---

A provider teaches the filesystem one external system. Each one ships as a
sandboxed `wasm32-wasip2` component and mounts under a path prefix you choose
(the defaults are below). The host owns trust, caching, and auth; the provider
owns meaning. To author your own, start with [the two flavours](the-two-flavours.md)
and the [authoring guide](authoring-guide.md).

## Shipped today

| Provider | Default mount | What it projects |
|---|---|---|
| GitHub | `/github` | Repositories, issues, pull requests, and Actions; repo trees hand off as real directories. |
| Docker | `/docker` | Containers, images, and their state through the local Docker socket. |
| arXiv | `/arxiv` | Papers as JSON, PDF, and source, with versions and category listings; no auth. |
| Linear | `/linear` | Teams and issues with their fields, over the Linear GraphQL API. |
| DNS | `/dns` | Records and reverse lookups over DoH; structural, no auth. |
| Database | `/db` | A local SQLite database's schema, row counts, and sample rows, read-only. |

Per-provider reference pages (the full path tour, declared capabilities, auth
schemes, and cache behaviour for each) are generated from each provider's
`omnifs.provider.json` manifest and route table. They are being ported into this
section; until then, the manifest in the repo is the exact source of truth for
what each provider declares.

## Mounting one

Configure a mount with `omnifs init <provider>`, or add a `[[mounts]]` entry to
`~/.omnifs/config.toml`. See [install and first read](../get-oriented/install-and-first-read.md)
for the first-run walkthrough and [the trust model](../security/the-trust-model.md)
for what a provider can and cannot do once mounted.
