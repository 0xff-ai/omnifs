---
title: Reference
description: Generated reference index; each page is produced from a named source artifact so it cannot drift from the build.
---

Reference is generated from source, not hand-written, so it cannot drift from the build. Each page below is produced from the artifact named beside it; this index is the map. The prose sections explain concepts; these pages state the exact shape.

| Page | Generated from |
|---|---|
| CLI | the clap command definitions and command modules |
| Config schema | the mount and provider config schema types |
| Path schemes | each provider's route registrations (code wins over README) |
| Provider manifest | the `omnifs.provider.json` schema and shipped manifests |
| Runtime grants | the resolved capability shape after mount resolution |
| WIT interface | `crates/omnifs-wit/wit/provider.wit` (`omnifs:provider@0.4.0`) |
| File attributes | the projection attribute types and the file-attributes design |
| SDK API | the `omnifs-sdk` rustdocs, organized by author-facing concept |
| Cache semantics | the object-cache design and the cache crate |
| Capability types | the capability checker and provider manifests |
| Error model | the WIT and SDK error types and their conversions |
| Environment variables | the CLI, session, and runtime sources |
| Glossary | the shared terminology registry |
| Shell compatibility matrix | the bash-tool compatibility list and the smoke tests |

Across every reference page, code wins when a README or design note disagrees with it, and a generated page states what the build does today, including where coverage is partial, not what the design intends.
