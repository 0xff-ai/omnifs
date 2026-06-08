# omnifs-provider-arxiv

[omnifs](https://github.com/0xff-ai/omnifs) provider that projects [arXiv](https://arxiv.org) papers into a FUSE-visible tree. Address a paper by id; each paper is a version family whose `@latest` alias and numbered `vN` directories expose metadata, the raw Atom feed, the PDF, and the e-print source blob.

## Mount layout

```
/arxiv/
  papers/{id}/
    @latest/
      paper.json
      paper.atom
      paper.pdf        latest PDF
      source.tar.gz    latest source bundle
    v{n}/
      paper.json
      paper.atom
      paper.pdf        version-pinned PDF
      source.tar.gz    version-pinned source bundle
  categories/{category}/papers/{id}/
    @latest/
    v{n}/
```

A paper is an object whose canonical is the upstream Atom feed: `paper.atom` serves it verbatim and `paper.json` renders a lossy metadata view (title, authors, categories, DOIs, resource URLs). `@latest` is mutable because it can move when arXiv publishes a new version; numbered `vN` directories are immutable once the paper feed reports that version.

`{id}` accepts modern ids like `2401.12345` directly. Old-style ids must use a single encoded path segment, for example `cs.LG%2F0512345`; versioned ids such as `2401.12345v2` are not accepted in `{id}` and must be accessed as `2401.12345/v2/...`.

Category recent listings are exposed under `/categories/{category}/papers`; member paper subtrees have the same version-first shape as `/papers/{id}`.

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
