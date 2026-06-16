---
title: Use cases
description: "Concrete workflows omnifs supports: built-in caches, warm reads, agent context, traceability, and pipes across system boundaries."
---

omnifs is designed for workflows where APIs and data sources should be available through the same filesystem interface as the local work that consumes them. The key property is not a new client per service. The key property is that each mounted resource can be read through paths.

These workflows assume the relevant providers are configured and the runtime is running.

## Read through the built-in caches

When a process reads a path, omnifs owns the fetch, projection, and cache boundary. A first read can fetch upstream through a host-mediated callout. Later reads can hit the view cache, render from cached canonical object bytes, or serve large blobs from the blob cache.

```bash
cat /omnifs/github/0xff-ai/omnifs/issues/open/42/item.json > context/issue.json
cat /omnifs/arxiv/papers/1706.03762/paper.json > context/paper.json
```

There is no separate cache service to configure. The cache is part of the projected filesystem's read path. Running the runtime near a tool, worker, or agent is a placement choice for latency and locality, not a separate caching protocol.

## Warm offline reads

When the data needed for a path is already in the host cache and the provider can render from cached canonical bytes, reads keep working without an upstream call. This supports repeat inspection, local debugging, and agent runs over recently fetched context.

This is warm-cache behavior, not an explicit offline snapshot facility.

## Reduce context handed to agents

Instead of pasting API schemas, examples, and raw payloads into an agent prompt, point the agent at paths:

```text
/omnifs/github/0xff-ai/omnifs/issues/open/42/item.md
/omnifs/github/0xff-ai/omnifs/repo
/omnifs/linear/teams/ENG/issues/open/ENG-1421/item.md
```

This can reduce the amount of bespoke context you need to hand to the agent. Treat token savings as a workflow possibility, not a measured guarantee, unless your own run records the before and after.

## Trace a read

Use `omnifs inspect` when you need evidence for what happened:

```bash
omnifs inspect --plain
cat /omnifs/dns/example.com/MX
```

The inspector shows local runtime events: provider calls, callouts, cache behavior, and errors. This gives traceability for debugging and trust, but it is not compliance-grade audit logging.

## Pipe data and state across system boundaries

Once systems are projected on the same surface, shell tools can connect them without a bespoke integration for every pair.

```bash
cat /omnifs/github/0xff-ai/omnifs/issues/open/42/title
cat /omnifs/linear/teams/ENG/issues/open/ENG-1421/state
cat /omnifs/docker/containers/by-name/api/summary.txt
```

Cross-system workflows depend on the providers mounted in the runtime. Examples that reference absent providers are sketches, not usable paths.
