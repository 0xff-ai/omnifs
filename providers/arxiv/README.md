# omnifs-provider-arxiv

[omnifs](https://github.com/0xff-ai/omnifs) provider that projects [arXiv](https://arxiv.org) papers into a FUSE-visible tree. Address a paper by id; each paper is an object that materializes its metadata, the raw Atom feed, the PDF and e-print source blobs, and a version tree.

## Mount layout

```
/arxiv/
  papers/{id}/
    paper.json      rendered metadata
    paper.atom      verbatim Atom canonical
    paper.pdf
    source.tar.gz
    versions/v{n}/
      paper.json
      paper.pdf
      source.tar.gz
```

A paper is an object whose canonical is the upstream Atom feed: `paper.atom` serves it verbatim and `paper.json` renders a lossy metadata view (title, authors, categories, DOIs, resource URLs).

`{id}` accepts modern ids like `2401.12345` directly. Old-style ids must use a single encoded path segment, for example `cs.LG%2F0512345`; `versions/v{n}/` re-projects metadata and the resource blobs at a specific version.

The category/recent/submissions browse surface is not exposed in this release; papers are addressed by id under `/papers/`.

## Capabilities

Network access to `export.arxiv.org` and `arxiv.org`. 64 MiB memory limit. Read-only.

## Install

This is a wasm component. Build with:

```bash
cargo build --target wasm32-wasip2 --release -p omnifs-provider-arxiv
```

The resulting `omnifs_provider_arxiv.wasm` is also attached to each [GitHub Release](https://github.com/0xff-ai/omnifs/releases). Configure in your omnifs mount config under the `arxiv` key.

## Status

Pre-1.0. Mount layout may evolve.

## License

Dual licensed under MIT or Apache-2.0 at your option.
