---
title: Packaging
description: How provider builds produce wasm32-wasip2 components and how built-in providers are indexed from their embedded manifests.
---

Providers build as `wasm32-wasip2` components. The provider manifest is embedded in the component at compile time.

## Local build

Use provider checks while authoring:

```bash
just providers-check
just providers-build
```

Provider tests compile for `wasm32-wasip2`, but WASM tests do not execute in the normal host test harness. Use host-compatible unit tests for logic that needs to run locally.

## Provider indexing

Providers packaged with omnifs are indexed from their embedded manifests. The host reads the manifest from the component to discover the provider id, capabilities, auth schemes, and default mount name. Use the provider catalogue for documented path surfaces.

The test provider is internal test infrastructure, not a documented provider surface.

## Third-party distribution

Standalone third-party provider packaging and publishing is still stabilizing. There is no public npm publishing path for providers yet.

For now, treat provider packaging docs as component build docs. Standalone third-party provider distribution is not a supported public workflow.
