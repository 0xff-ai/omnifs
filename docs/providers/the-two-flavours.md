---
title: The two flavours
description: The object-oriented and path-oriented provider shapes, when to use each, and why most real providers are hybrids.
---

Providers come in two shapes, and choosing the right one is the first design decision you make.

An object-oriented provider has a canonical upstream payload behind each resource. A GitHub issue is one JSON document. From it you derive a title, a body, a state, and a rendered `item.md`. You write the resource as an object: declare a key that fetches the canonical payload, then declare the leaves that project from it. The SDK stores the canonical bytes in the object cache and re-renders the leaves from them, so a second read costs nothing upstream. Reach for this when there is one upstream truth that several files describe.

A path-oriented provider answers directly, with no durable object behind the path. A DNS query, a `docker ps`, a database row count: each is a fresh operation whose result you return with an accurate freshness declaration. There is no canonical payload to cache forever, so you do not pretend there is one. Reach for this for queries and live state.

Most real providers are hybrids. GitHub stores issues, pull requests, and runs as objects, but lists filters and owners path-style and hands off the repository as a tree. The two flavours are not exclusive. A provider uses whichever fits each route.

One thing to avoid is adding an object just to get caching. Without a canonical upstream payload, stay path-oriented and declare accurate stability. Add an object only when the upstream really is a single payload, not because the cache is convenient.
