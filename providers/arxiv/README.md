# omnifs-provider-arxiv

[omnifs](https://github.com/0xff-ai/omnifs) provider that projects [arXiv](https://arxiv.org) papers into a FUSE-visible tree. Browse recent category submissions or direct paper ids; each paper materializes its PDF, source tarball, metadata, links, and version history as files.

## Mount layout

```
/arxiv/
  papers/{id}/
    paper.pdf
    source.tar.gz
    metadata.json
    links.json
    versions/v{n}/
  categories/{cat}/recent/
  categories/{cat}/recent/fetched/
  categories/{cat}/recent/pages/
  categories/{cat}/recent/pages/{n}/
  categories/{cat}/submissions/
  categories/{cat}/submissions/{YYYYMMDD}/
```

`{id}` accepts modern ids like `2401.12345` directly. Old-style ids must use a single encoded path segment, for example `cs.LG%2F0512345`; `versions/v{n}/` re-projects the paper subtree at a specific version.

Category traversal uses arXiv's recent category feed with `search_query=cat:{cat}` and `max_results=100`.
Results are sorted descending by `sortBy=submittedDate`.
`recent/pages/{n}` fetches upstream pages, while `recent/fetched` is the deduped set discovered so far.
Submission-day directories are materialized from already fetched recent pages and never issue date-range queries.

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
