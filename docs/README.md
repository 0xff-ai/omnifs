# omnifs documentation

Content for the omnifs docs site. The section order and labels are the source of
truth in [`_nav.ts`](_nav.ts); page order within a section comes from each page's
frontmatter. Voice, reserved terms, and the honesty gates are governed by
[`CLAUDE.md`](CLAUDE.md).

Four sections name the system by its architecture (projections, engine, providers,
surfaces); the rest are topical (get oriented, recipes, tutorials, security and
trust, reference, project). The structure leads with the architecture because that
is the mental model: a systems-minded reader masters the machine one layer at a
time, while the Get oriented funnel and the Recipes cookbook give a do-first reader
a short path to a working command.

## Architecture map

The four architecture sections trace the data path, top to bottom:

```
        consumers — shell · scripts · agents · apps
                  │  open / read / readdir
                  ▼
   Surfaces       FUSE today · NFSv4 / FSKit later
                  │
                  ▼
   Projections    paths · objects · rendered views · trees
                  ▲
                  │  serves
   Engine         routing · identity · cache · auth · callouts
                  │  callouts
                  ▼
   Providers      sandboxed WASM — supply meaning
                  │
                  ▼
        external systems
```

The engine serves the projection (it routes, caches, authorizes); providers,
through the SDK, render it. "Render" is reserved for canonical object to format
(markdown, yaml, json). The IA enforces one concept per page in one home, with no
page name repeated across sections; the resolved naming knots (callouts-and-effects
vs reaching-upstream, the single homes for file attributes and capabilities) are
recorded in [`CLAUDE.md`](CLAUDE.md).

This tree holds the ported pages (step 1 of the build order: spartan bodies and
branch concept prose, with frontmatter). Pages still to write (cross-section
merges and net-new) are listed under "Pending" at the end.

## Get oriented
- [What omnifs is](get-oriented/what-omnifs-is.md)
- [Why paths](get-oriented/why-paths.md)
- [Use cases](get-oriented/use-cases.md)
- [Install and first read](get-oriented/install-and-first-read.md)
- [Setup and troubleshooting](get-oriented/setup-and-troubleshooting.md)
- [Limits and non-goals](get-oriented/limits.md)

## Projections (the omnifs view of the world)
- [Paths as the interface](projections/paths-as-the-interface.md)
- [The browse surface](projections/the-browse-surface.md)
- [Subtree handoff](projections/subtree-handoff.md)
- [What files report](projections/what-files-report.md)

## Engine (omnifs internals)
- [The cache trilogy](engine/the-cache-trilogy.md)
- [Callouts and effects](engine/callouts-and-effects.md)
- [Auth and credential custody](engine/auth-and-credential-custody.md)

## Providers (the supply side)
- [Provider catalogue](providers/index.md)
- [The two flavours](providers/the-two-flavours.md)
- [Authoring guide](providers/authoring-guide.md)
- [Routing and objects](providers/routing-and-objects.md)
- [Config, manifests, and capabilities](providers/config-manifests-and-capabilities.md)
- [Reaching upstream](providers/reaching-upstream.md)
- [Testing and debugging](providers/testing-and-debugging.md)
- [Packaging](providers/packaging.md)

## Surfaces (how the projection reaches the OS)
- [Shell compatibility](surfaces/shell-compatibility.md)

## Recipes
- [Read and inspect](recipes/read.md)
- [Search and traverse](recipes/search.md)
- [Stat and measure](recipes/stat.md)
- [Copy and archive](recipes/copy-and-archive.md)
- [Compare and hash](recipes/compare-and-hash.md)
- [Inspect structured data](recipes/structured.md)
- [Cross-service pipes](recipes/cross-service-pipes.md)
- [make and pipelines](recipes/make-and-pipelines.md)
- [CI and headless](recipes/ci-and-headless.md)
- [Use omnifs with local agents](recipes/with-local-agents.md)
- [Build on the namespace](recipes/build-on-the-namespace.md)

## Tutorials
- [Mount GitHub and inspect issues](tutorials/mount-github-and-inspect-issues.md)
- [Build a tiny provider](tutorials/build-a-tiny-provider.md)
- [Add auth to a provider](tutorials/add-auth-to-a-provider.md)
- [Add object and view caching](tutorials/add-object-and-view-caching.md)
- [Expose a local SQLite database](tutorials/expose-a-local-sqlite-database.md)

## Security and trust
- [The trust model](security/the-trust-model.md)

## Reference (generated; hand-written interim)
- [Reference index](reference/index.md)
- CLI, config schema, path schemes, provider manifest, runtime grants, WIT,
  file attributes, SDK, cache, capability types, errors, environment, glossary,
  shell-compatibility matrix (see the `reference/` directory)

## Project
- [Roadmap](project/roadmap.md)
- [Distribution](project/distribution.md)
- [Contributing](project/contributing.md)
- [Design decisions](project/design-decisions.md)
- [FAQ](project/faq.md)

## Pending (steps 2 and 3)

Cross-section merges and net-new pages, not yet written. Each is sourced from a
shipped spartan page, an existing branch draft, or a design doc under
`docs/_dev/design/`; prior branch drafts are in git history under the old
`plane-*` paths.

- Get oriented: the architecture map (redrawn to the four areas).
- Projections: objects, fields, and rendered views; agent legibility.
- Engine: the daemon and control API; routing and identity; the capability
  broker; the inspector.
- Providers: what a provider is and is not; catalogue and per-provider pages
  (github, docker, arxiv, linear, dns, db).
- Surfaces: section intro; FUSE; NFSv4 and FSKit; mounting; platform support.
- Security and trust: audit and observability; worldviews; team and enterprise
  operation.
- Project: distribution; release; contributing; design decisions; FAQ.
