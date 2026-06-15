---
title: Limits and comparisons
description: What omnifs supports today, what remains future work, and how it differs from API clients, sync tools, ETL, and MCP-style tool surfaces.
---

omnifs is deliberately narrow. It projects APIs and data sources into a filesystem namespace, with the host controlling authority, cache, and side effects.

## Limits

| Area | Status |
|---|---|
| Writes | Read operations are the supported operation class. |
| Native non-Linux surfaces | Linux FUSE is the runtime surface. macOS uses the Linux container and `omnifs shell`. |
| Full offline snapshots | Warm cached reads exist; explicit offline snapshots are future work. |
| Hosted or edge runtime | Speculative. The runtime is local/container-oriented. |
| Provider distribution | Providers packaged with omnifs are listed in the provider catalogue. Standalone third-party publishing is still stabilizing. |
| POSIX coverage | The target is normal Unix tooling. Current automated proof is narrower than the full compatibility ambition. |

## Compared to API clients

An API client gives a program typed calls into one service. omnifs gives tools paths into projected resources.

Use an API client when the program owns all control flow and wants service-specific operations. Use omnifs when shell tools, scripts, local agents, or humans should read projected resources without embedding each service client.

## Compared to sync tools

A sync tool copies or mirrors a selected dataset. omnifs resolves paths on demand and caches what the host has learned.

Use sync when you need a durable offline copy of a known tree. Use omnifs when you need a projection over live API-backed resources, with warm local reads where the cache can serve them.

## Compared to ETL

ETL jobs move data from source to destination through a pipeline. omnifs does not replace batch movement or warehouse modeling.

Use ETL when the goal is a transformed dataset with scheduled ownership. Use omnifs when the goal is local inspection, composition, and tool access over APIs and data sources.

## Compared to MCP-style tools

MCP-style tools expose named actions to an agent. omnifs exposes paths that any filesystem-aware process can inspect.

These can coexist. A tool can call omnifs paths, and an agent can use both an MCP surface and projected paths. The difference is the interface: omnifs makes the filesystem the stable surface.
