# arXiv version-first provider surface

Status: implemented

## Summary

The arXiv provider models a paper as a version family. Every readable paper
artifact belongs to either the mutable `@latest` alias or an immutable numbered
version such as `v1`. There are no direct files under the paper directory.

The surface is:

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

Their logical id has kind `arxiv.paper` and a single identity capture `paper`
(the `version` facet is excluded from identity):

```text
arxiv.paper|paper=2604.00002
```

That pipe rendering is illustrative shorthand. The wire form is a `logical-id`
record (`kind: string`, `captures: list<id-capture>`), not a delimited string.

The provider still owns path-to-object mapping. The host only learns the
logical id and view leaves from provider effects.

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

There are no direct `paper.*` or `source.tar.gz` leaves under `{paper}`, and no
`versions/` subtree. Every readable artifact lives under a version selector. A
missing direct file or `versions` path is a normal not-found result.

## Canonical and representation behavior

`paper.atom` is the raw canonical Atom feed. The provider serves it verbatim
from the canonical store when the host pushes cached canonical bytes. It is not
an explicit `o.file` leaf: `o.representations("paper", ())` registers the
canonical source leaf, and because the object's canonical content type is `Atom`
the leaf stem resolves to `paper.atom`.

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

Blob cache keys are version-scoped, so `@latest` (unpinned arXiv URLs) and `vN`
(version-pinned arXiv URLs) never collide:

```text
arxiv/papers/{paper}/latest/paper.pdf
arxiv/papers/{paper}/v1/paper.pdf
arxiv/papers/{paper}/latest/source.tar.gz
arxiv/papers/{paper}/v1/source.tar.gz
```

## State and caching

The provider keeps no provider-local `HashMap` of papers. The host object cache
is the canonical cache for paper metadata.

If a handler needs paper metadata, it loads through the normal `Key::load` path
and relies on the host-pushed canonical when available. There is no provider-side
paper memoization.
