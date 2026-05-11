# omnifs-provider-arxiv

[omnifs](https://github.com/raulk/omnifs) provider that projects [arXiv](https://arxiv.org) papers into a FUSE-visible tree. Browse papers by id, by category, by author, or by free-text search; each paper materializes its PDF, source tarball, metadata, and version history as files.

## Mount layout

```
/arxiv/
  papers/{id}/
    paper.pdf
    source.tar.gz
    metadata.json
    links.json
    versions/v{n}/
  categories/{cat}/{YYYY}/{MM}/{DD}/
  categories/{cat}/{new | updated | by-author}/
  authors/{author}/{... | by-category}/
  search/{query}/
```

`{id}` accepts both the modern `2401.12345` and legacy `cs.LG/0512345` formats. `versions/v{n}/` re-projects the paper subtree at a specific version.

Category calendar listings are day-bounded to keep each arXiv API query small. If a listing returns fewer papers than arXiv reports for that scope, the directory includes `_more` with a short `fetched listed/total` marker.

## Capabilities

Network access to `export.arxiv.org` and `arxiv.org`. 64 MiB memory limit. Read-only.

## Install

This is a wasm component. Build with:

```bash
cargo build --target wasm32-wasip2 --release -p omnifs-provider-arxiv
```

The resulting `omnifs_provider_arxiv.wasm` is also attached to each [GitHub Release](https://github.com/raulk/omnifs/releases). Configure in your omnifs mount config under the `arxiv` key.

## Status

Pre-1.0. Mount layout may evolve.

## License

Dual licensed under MIT or Apache-2.0 at your option.
