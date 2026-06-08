# arXiv version-first provider surface

Status: implemented

## Summary

The arXiv provider should model a paper as a version family. Every readable
paper artifact belongs to either the mutable `@latest` alias or an immutable
numbered version such as `v1`. Direct files under the paper directory are
removed.

The target surface is:

```text
/papers/{paper}/
  @latest/
    paper.atom
    paper.json
    paper.pdf
    source.tar.gz
  v1/
    paper.atom
    paper.json
    paper.pdf
    source.tar.gz
  v2/
    ...

/categories/{category}/papers/{paper}/
  @latest/
  v1/
  v2/
```

`/papers/{paper}` is a directory for the version family. It lists `@latest`
plus `v1..=latest_version` after loading the paper Atom. Category aliases mount
the same version-family surface under a pure-navigation prefix; the category is
not part of paper identity.

## Rationale

The previous arXiv surface mixed two meanings in one directory. Files such as
`/papers/{paper}/paper.pdf` meant "latest version", while
`/papers/{paper}/versions/v1/paper.pdf` meant a concrete version. That forced
the provider to carry special latest-vs-version rules and made direct paper
children look like a different resource class than numbered version children.

The version-first surface removes that ambiguity:

- `@latest` is a mutable alias to the latest version known from the Atom feed.
- `vN` is an immutable version selector.
- The paper directory is only the version-family root.
- `paper.atom`, `paper.json`, `paper.pdf`, and `source.tar.gz` always live under
  a version selector.

This shape is easier to explain and gives every readable artifact one obvious
version context.

## SDK model

The provider remains hybrid:

- The paper metadata is object-oriented: one `Paper` object, one stable
  `PaperKey`, one canonical Atom body, and derived metadata representations.
- Category listing and PDF/source downloads stay path-oriented: category listing
  returns paper ids only, while PDF/source paths fetch blobs from `arxiv.org`.

The object anchor is version-contextual in the route but version-independent in
identity:

```rust
#[omnifs_sdk::path_captures]
struct PaperVersionKey {
    paper: PaperId,
    version: Facet<PaperVersion>,
}
```

`PaperVersion` is a route facet, not identity. Therefore these paths all resolve
to the same logical id:

```text
/papers/2604.00002/@latest/paper.atom
/papers/2604.00002/v1/paper.atom
/categories/cs.AI/papers/2604.00002/v1/paper.atom
```

Their logical id is:

```text
arxiv.paper|paper=2604.00002
```

The provider still owns path-to-object mapping. The host only learns the
`LogicalId` and view leaves from provider effects.

## Route contract

### Version family root

`/papers/{paper}` and `/categories/{category}/papers/{paper}` are directories.
They list:

```text
@latest
v1
v2
...
v{latest_version}
```

The directory load fetches the paper Atom if the host cannot push cached
canonical bytes. The listing is exhaustive once the Atom is loaded.

### Latest alias

`@latest` always exists for a valid paper:

```text
/{paper}/@latest/paper.atom
/{paper}/@latest/paper.json
/{paper}/@latest/paper.pdf
/{paper}/@latest/source.tar.gz
```

`@latest/*` leaves are mutable because the upstream paper can receive a new
version and the alias can move.

### Numbered versions

`vN` is valid only when `1 <= N <= latest_version` from the loaded Atom.

```text
/{paper}/vN/paper.atom
/{paper}/vN/paper.json
/{paper}/vN/paper.pdf
/{paper}/vN/source.tar.gz
```

`vN/*` leaves are immutable.

### Removed paths

The following paths are removed:

```text
/{paper}/paper.atom
/{paper}/paper.json
/{paper}/paper.pdf
/{paper}/source.tar.gz
/{paper}/versions
/{paper}/versions/vN
/{paper}/versions/vN/*
```

No compatibility alias is kept in this rewrite. A missing direct file or
`versions` path should be a normal not-found result.

## Canonical and representation behavior

`paper.atom` is the raw canonical Atom feed. The provider serves it verbatim
from the canonical store when the host pushes cached canonical bytes.

`paper.json` is derived from the `Paper` value. For `@latest`, JSON uses the
latest version number from the Atom. For `vN`, JSON uses `N` when building
version-specific resource URLs.

The same canonical Atom backs both `@latest` and `vN` reads. The provider does
not pretend arXiv exposes historical per-version Atom bodies when it only has
the current Atom feed. Version-specific metadata is a derived projection of the
current paper metadata plus a concrete version selector.

## Blob behavior

`paper.pdf` and `source.tar.gz` are blob-backed path reads:

- `@latest/paper.pdf` fetches `/pdf/{paper}.pdf` and is mutable.
- `@latest/source.tar.gz` fetches `/e-print/{paper}` and is mutable.
- `vN/paper.pdf` fetches `/pdf/{paper}vN.pdf` and is immutable.
- `vN/source.tar.gz` fetches `/e-print/{paper}vN` and is immutable.

Blob cache keys should include the version selector:

```text
arxiv/papers/{paper}/latest/paper.pdf
arxiv/papers/{paper}/v1/paper.pdf
arxiv/papers/{paper}/latest/source.tar.gz
arxiv/papers/{paper}/v1/source.tar.gz
```

## State and caching

The provider should not keep a provider-local `HashMap` of papers. The host
object cache is the canonical cache for paper metadata.

If a handler needs paper metadata, it should load through the normal `Key::load`
path and rely on the host-pushed canonical when available. No provider-side
paper memoization should survive the rewrite.

## Tests

Provider integration tests should assert the product contract rather than the
old internal handler choreography:

- attach symmetry: direct and category paths produce the same logical id
- canonical source: `paper.atom` is raw Atom
- derived JSON: `paper.json` is JSON and not raw Atom
- version root listing: `@latest` plus `v1..=latest_version`
- latest blobs are mutable and use unpinned arXiv URLs
- numbered blobs are immutable and use version-pinned arXiv URLs
- old direct files are missing
- old `versions/` paths are missing
- old-style encoded ids still round-trip
- versioned ids in `{paper}` are rejected; users must use `/vN`
- category listings emit paper dirs without member canonicals
- category pagination still works

## Implementation checklist

- [x] Add this design document.
- [x] Replace `Version` with a version selector type that parses `@latest` and
      `vN`.
- [x] Introduce `PaperVersionKey { paper, version: Facet<PaperVersion> }`.
- [x] Make `PaperVersionKey` the `Paper` object key.
- [x] Keep identity version-independent by making `version` a `Facet`.
- [x] Remove provider-local `State.papers`.
- [x] Replace direct paper object attach with version-context object attach.
- [x] Register `/papers/{paper}` and `/categories/{category}/papers/{paper}` as
      version-family directory handlers.
- [x] Register object leaves under `/{paper}/{version}/`.
- [x] Remove direct `paper.*` and `source.tar.gz` leaves under `{paper}`.
- [x] Remove `versions/{version}` leaves.
- [x] Validate numbered versions against `latest_version`.
- [x] Mark `@latest` leaves mutable.
- [x] Mark `vN` leaves immutable.
- [x] Keep category listing path-oriented and canonical-free.
- [x] Update provider README route examples.
- [x] Update arXiv integration tests to the version-first paths.
- [x] Rebuild `omnifs_provider_arxiv.wasm`.
- [x] Run targeted arXiv integration tests.
- [x] Run provider wasm check/clippy for arXiv.
