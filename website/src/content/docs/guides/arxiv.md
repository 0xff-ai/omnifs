---
title: Browsing arXiv
description: Read arXiv papers as directories under /arxiv — PDFs, LaTeX source, metadata, versions, and category/author/search scopes.
---

The arXiv provider mounts under `/arxiv`. Each paper is a directory; its
metadata, PDF, and source are files. The same per-paper subtree appears under
every scope (categories, authors, search), so the file shape is identical
wherever you reach a paper from.

```bash
ls /arxiv/papers/1706.03762
cat /arxiv/papers/1706.03762/metadata.json
```

## Path map

| Path | Content |
|------|---------|
| `/arxiv/papers/{id}/` | Per-paper subtree (any arXiv id, e.g. `1706.03762`) |
| `/arxiv/papers/{id}/paper.pdf` | Latest version PDF |
| `/arxiv/papers/{id}/source.tar.gz` | Latest version source bundle |
| `/arxiv/papers/{id}/metadata.json` | Title, authors, abstract, categories, links |
| `/arxiv/papers/{id}/links.json` | Resolved arXiv URLs for this paper / version |
| `/arxiv/papers/{id}/versions/v{n}/{paper.pdf,…}` | Same files for a specific version |
| `/arxiv/categories/{cat}/{YYYY}/{MM}/{DD}/` | Papers in `cat` posted on that UTC day |
| `/arxiv/categories/{cat}/new/` | Most-recent papers in `cat` by submitted date |
| `/arxiv/categories/{cat}/updated/` | Most-recent papers in `cat` by last-updated date |
| `/arxiv/categories/{cat}/by-author/{author}/` | Papers in `cat` by `author` |
| `/arxiv/authors/{author}/` | Papers by author (same `new`/`updated`/`by-category` axes) |
| `/arxiv/search/{query}/` | arXiv search results (URL-encoded query) |

## A single paper

A paper directory under `/arxiv/papers/{id}/` holds everything for that paper.

```bash
ls  /arxiv/papers/1706.03762
cat /arxiv/papers/1706.03762/metadata.json   # title, authors, abstract, categories
cat /arxiv/papers/1706.03762/links.json      # resolved arXiv URLs
cp  /arxiv/papers/1706.03762/paper.pdf ~/attention.pdf
cp  /arxiv/papers/1706.03762/source.tar.gz ~/attention-src.tar.gz
```

## Versions

Each version is exposed under `versions/v{n}/` with the same file shape as the
paper root.

```bash
ls  /arxiv/papers/1706.03762/versions
cp  /arxiv/papers/1706.03762/versions/v1/paper.pdf ~/attention-v1.pdf
cat /arxiv/papers/1706.03762/versions/v2/metadata.json
```

## Scopes: categories, authors, search

Browse collections of papers by category, by author, or by search query. Each
scope lists papers you can descend into — the per-paper subtree is the same as
under `/arxiv/papers/{id}/`.

```bash
# Papers in a category, by day or by recency
ls /arxiv/categories/cs.AI/2024/01/31
ls /arxiv/categories/cs.AI/new
ls /arxiv/categories/cs.AI/updated

# A category narrowed to one author
ls /arxiv/categories/cs.CL/by-author/Vaswani

# All papers by an author
ls /arxiv/authors/Vaswani

# Search results (URL-encoded query)
ls /arxiv/search/transformer
```

:::tip
Combine arXiv with the inspection tools — `jq` reads the metadata files
directly, so you can extract just the fields you need:

```bash
jq -r '.title' /arxiv/papers/1706.03762/metadata.json
```
:::
