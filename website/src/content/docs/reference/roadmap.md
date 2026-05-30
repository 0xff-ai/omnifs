---
title: Roadmap
description: Planned work for the omnifs core and the provider roadmap. Everything here is planned, not shipped.
---

This page lists planned and exploratory work. omnifs is very alpha; treat everything below
as direction, not commitment. For what works today, see
[Project status](/introduction/project-status/) and the
[provider guides](/guides/browsing/).

:::caution
Nothing on this page is shipped. These are intended directions drawn from the project's
"What's coming" notes. Names, scope, and ordering may change.
:::

## Core omnifs

Planned work on the runtime and platform:

- **Write-back via Git push** — mutations applied through staged Git transactions rather
  than direct writes. See [Future design](/reference/future/) for the mutations-via-Git
  model.
- **Better caching** — hot-path memoization, negative caching, and smarter invalidation.
- **Background indexing** — for large trees and expensive projections.
- **Search** — across projected content, metadata, and repo history.
- **Tracing and observability** — for provider calls, cache behavior, and FUSE latency.
- **Better prefetching and pagination** — strategies for large orgs and large repos.
- **Persistent inode stability** — stable inodes across remounts.
- **Offline-friendly local snapshots** — and replayable sync.
- **Mutation workflows** — beyond today's read-only browsing.
- **macOS and Windows support** — the runtime mount is Linux-only today; macOS and Windows
  are planned. See [Platform notes](/getting-started/platform-notes/).

## Provider roadmap

New providers and deeper coverage in existing ones. The shipped providers today are
GitHub, DNS, and arXiv.

| Provider              | What it could project                                                                  |
| --------------------- | -------------------------------------------------------------------------------------- |
| GitHub                | Commits, branches, reviews, checks, releases, and discussion state.                    |
| Hugging Face          | Models, datasets, spaces, cards, files, versions, and download metadata as trees.      |
| Linear                | Teams, projects, issues, cycles, comments, labels, and workflow state with drafts.     |
| DNS                   | Zones, records, history, propagation state, and provider-backed change transactions.   |
| S3 and object stores  | Buckets, prefixes, object metadata, versions, lifecycle rules, and event streams.      |
| OCI registries        | Images, tags, manifests, layers, SBOMs, and signature material as mountable content.   |
| Kubernetes            | Clusters, namespaces, workloads, logs, events, and live resource views.                |
| Postgres and SQLite   | Schemas, tables, rows, views, and queryable virtual files for inspection and export.   |
| Slack and Discord     | Channels, threads, message history, attachments, and searchable conversation snapshots.|

:::note
Many roadmap providers depend on shared core work — mutations, search, and pagination —
landing first. The provider list and the core list move together.
:::
