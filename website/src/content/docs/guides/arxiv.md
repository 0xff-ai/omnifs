---
title: Browsing arXiv
description: Read arXiv papers as directories under /arxiv — PDFs, LaTeX source, metadata, versions, and category/author/search scopes.
---

The arXiv provider mounts under `/arxiv`. Each paper is a directory; its
metadata, PDF, and source are files. Scopes let you browse by category, author,
or search query.

```bash
ls /arxiv/papers/1706.03762
cat /arxiv/papers/1706.03762/metadata.json
```

## Path map

| Path | Content |
|------|---------|
| `/arxiv/papers/<id>/paper.pdf` | The paper PDF |
| `/arxiv/papers/<id>/source.tar.gz` | LaTeX source archive |
| `/arxiv/papers/<id>/metadata.json` | Title, authors, abstract, dates |
| `/arxiv/papers/<id>/links.json` | Related links (DOI, code, etc.) |
| `/arxiv/papers/<id>/versions/v<n>/...` | A specific version's files |
| `/arxiv/categories/<cat>` | Papers in a category |
| `/arxiv/authors/<name>` | Papers by an author |
| `/arxiv/search/<query>` | Search results |

## A single paper

A paper directory under `/arxiv/papers/<id>/` holds everything for that paper.

```bash
ls /arxiv/papers/1706.03762
cat /arxiv/papers/1706.03762/metadata.json   # title, authors, abstract, dates
cat /arxiv/papers/1706.03762/links.json      # DOI, code, related links
cp  /arxiv/papers/1706.03762/paper.pdf ~/attention.pdf
cp  /arxiv/papers/1706.03762/source.tar.gz ~/attention-src.tar.gz
```

## Versions

Each version is exposed under `versions/v<n>/` with the same file shape as the
paper root.

```bash
ls  /arxiv/papers/1706.03762/versions
cp  /arxiv/papers/1706.03762/versions/v1/paper.pdf ~/attention-v1.pdf
cat /arxiv/papers/1706.03762/versions/v2/metadata.json
```

## Scopes: categories, authors, search

Browse collections of papers by category, by author, or by search query. Each
scope lists papers you can then descend into.

```bash
ls /arxiv/categories/cs.CL          # papers in a category
ls /arxiv/authors/Vaswani           # papers by an author
ls /arxiv/search/transformer        # search results
```

:::tip
Combine arXiv with the inspection tools — `jq` reads the metadata files
directly, so you can extract just the fields you need:

```bash
jq -r '.title, .abstract' /arxiv/papers/1706.03762/metadata.json
```
:::
