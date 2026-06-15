---
title: Why paths
description: Why omnifs uses the filesystem as an interface for APIs and data sources instead of another SDK, API client, or universal schema.
---

omnifs uses paths so that APIs and data sources can be inspected without forcing every consumer to embed a service-specific client.

```bash
cat /github/0xff-ai/omnifs/issues/open/42/title
cat /linear/teams/ENG/issues/open/ENG-204/state
cat /docker/containers/by-name/api/state
cat /dns/cloudflare.com/MX
cat /arxiv/papers/1706.03762/paper.json | jq .title
cat /db/tables/Album/schema.sql
```

The caller does not initialize a service client. In the read path, it opens a file. Planned write flows preserve the same path-shaped interface while making mutation intent explicit and reviewable.

## The integration tax

Many operational workflows cross services. A repo lives in GitHub. The work queue lives in Linear. Runtime state lives in Docker or Kubernetes. Research lives in arXiv. Models live in registries. Customer state lives in databases, billing systems, docs, queues, and internal tools.

Each system asks the consumer to learn another shape: another SDK, another auth flow, another pagination model, another retry policy, another rate limit, another response format. That cost repeats for every consumer. Humans pay it in scripts and dashboards. Agents pay it in tool definitions, glue code, and context spent explaining APIs instead of inspecting state.

omnifs moves the integration boundary. Instead of teaching every consumer every API, each provider teaches the filesystem how one API or data source should be projected.

## The filesystem is the common layer

The common layer is not a universal schema. Universal schemas tend to collapse under the weight of real domains. A GitHub provider should preserve GitHub concepts. A Docker provider should preserve Docker concepts. A model registry provider should expose models, versions, cards, files, datasets, evals, and provenance in the shape used by that domain.

The common layer is the filesystem. Once resources are behind paths, existing tools can traverse them, read them, grep them, diff them, cache them, archive them, pipe them, and pass them to another process. The interface does not change when the consumer changes. A human, a script, a notebook, a CI job, and a local agent can all operate against the same path namespace.

## Providers preserve domain shape

omnifs does not pretend every service is storage. Issues have titles, bodies, comments, states, diffs, and users. DNS names have record files. Containers have state, logs, images, ports, mounts, and events. Tables have schemas, indexes, counts, and samples. Papers have metadata, PDFs, source bundles, and versions.

A provider owns that path grammar. It decides what paths exist, what they mean, and what bytes to return when the host asks for them. The host owns the privileged and cross-cutting responsibilities: filesystem behavior, credentials, caching, network access, Git handoffs, socket access, rate limits, provider lifecycle, and the authority boundary.

## Host-mediated providers

Providers are sandboxed `wasm32-wasip2` components. They implement the `omnifs:provider` WIT interface and answer filesystem operations such as `lookup-child`, `list-children`, and `read-file`.

A provider does not get ambient authority. It cannot open an arbitrary socket, read host files, inspect another provider, or see user credentials. When a provider needs an upstream resource, it suspends the current operation with declared callouts. The host checks the provider manifest, attaches credentials at the boundary, executes the callout, commits cache effects, and resumes the provider with the result.

Providers define the domain projection. The host owns authority, credentials, callout execution, cache commits, and runtime policy.

## Coverage

The [provider catalogue](/providers/) lists documented providers and their supported path families.

The provider model is intended to cover object stores, Kubernetes, model registries, Slack, Discord, Google Drive, Gmail, Notion, Postgres, Redis, Stripe, Cloudflare, Vercel, queues, feature flags, internal control planes, and company-specific APIs. The goal is not one global data model. The goal is one filesystem interface over many domain-specific projections.
