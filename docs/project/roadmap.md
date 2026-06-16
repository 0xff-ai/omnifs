---
title: Roadmap
description: What omnifs supports today, what is actively stabilizing, and what directions are planned or speculative.
---

# Roadmap

omnifs is a read-only projection. The roadmap focuses on making that projection reliable, the provider SDK usable, and the host boundary easy to inspect. Status is stated plainly: a capability is shipped, stabilizing, planned, or speculative, and this page does not promote one tier into another.

## Supported today

The Linux FUSE projection is the current runtime surface. macOS and Windows users reach it through `omnifs shell`, which attaches a container. The CLI ships as an npm package for both platforms.

Six providers are available: GitHub, Docker, arXiv, Linear, DNS, and db (SQLite). They are driven over the WIT contract as `wasm32-wasip2` components. The host manages all callouts, credentials, and capabilities; providers do not reach anything they were not explicitly granted.

The caching layer is fully operational: the object cache holds canonical upstream bytes durably, the view cache holds rendered representations without re-fetching, and the blob cache handles large binaries and archive trees. `omnifs inspect` traces a single read end to end for local observability.

## Actively stabilizing

Provider SDK ergonomics, object-shaped provider patterns, cache invalidation and trace clarity, and the CLI setup, auth, status, and doctor output are all in active work. Generated provider docs and the catalogue page depend on manifest and route-registration tooling that is being completed now.

## Planned

More providers. Standalone provider packaging and loading (so providers can be published and installed independently). Runtime frontends for environments where FUSE is not the right surface. Offline snapshots beyond warm cached reads. Signed provider manifests and richer capability display.

Do not document planned providers, runtime frontends, or write flows as supported behavior until their manifests, command paths, and examples exist.

## Writes

The read model stays read-only. When writes land, they are explicit and reviewable: intent is staged as a draft, then executed by a commit-shaped act that produces an auditable record. Writes are never an implicit side effect of writing to a projected file. This is not a workaround; it is a design decision, and nothing about it changes until the explicit write model is built.

## Speculative

Host-native non-Linux projections over NFSv4 and FSKit, offline snapshots with dated-snapshot diffing, semantic and search routes, hosted or edge runtimes, and named worldviews provisioned per consumer. These are directions, not commitments.

Open questions that do not yet have a design: native surface UX for macOS, hosted runtime shape, the write UX, provider distribution and provenance, and policy export for audits.
