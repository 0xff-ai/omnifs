---
title: Provider roadmap
description: What's coming next for omnifs providers — new domains to mount and deeper projections for the providers that already ship.
---

This page collects what is **planned but not yet shipped**. Everything here is direction, not a guarantee, and omnifs is alpha. For what works today, see the [provider catalog](/providers/).

## New providers

Future providers would each mount a new root and project its domain as files, the same way the shipped providers do.

| Provider | What it could project |
| --- | --- |
| Hugging Face | Models, datasets, spaces, cards, files, versions, and download metadata as browsable trees |
| S3 and object stores | Buckets, prefixes, object metadata, versions, lifecycle rules, and event streams |
| OCI registries | Images, tags, manifests, layers, SBOMs, and signature material as mountable content |
| Kubernetes | Clusters, namespaces, workloads, logs, events, and live resource views |
| Slack and Discord | Channels, threads, message history, attachments, and searchable conversation snapshots |

## Deeper projections for shipped providers

The providers that already exist are expected to grow more surface.

| Provider | Planned expansion |
| --- | --- |
| [GitHub](/providers/github/) | Commits, branches, reviews, checks, releases, and discussion state |
| [Linear](/providers/linear/) | Teams, projects, issues, cycles, comments, labels, and workflow state with draftable mutations |
| [DNS](/providers/dns/) | Zones, records, history, propagation state, and provider-backed change transactions |
| [Database](/providers/database/) | PostgreSQL support alongside SQLite; schemas, tables, rows, views, and queryable virtual files for inspection and export |

## Core omnifs work that providers depend on

Several planned capabilities live in the host, not in any single provider, but unlock richer provider behavior:

- Write-back via Git push — mutations through staged transactions, so editing files and renaming a transaction directory into a control namespace performs the API call.
- Better caching: hot-path memoization, negative caching, and smarter invalidation.
- Background indexing for large trees and expensive projections.
- Search across projected content, metadata, and repo history.
- Tracing and observability for provider calls, cache behavior, and FUSE latency.
- Better prefetching and pagination for large orgs and repos.
- Persistent inode stability across remounts.
- Offline-friendly local snapshots and replayable sync.
- macOS and Windows support.

:::note
Mutations are not implemented yet. The read model is read-only across all providers today; write-back is designed to flow through staged Git transactions rather than making projected files directly writable.
:::
