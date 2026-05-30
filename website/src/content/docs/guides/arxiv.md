---
title: Browsing arXiv
description: Read arXiv papers as directories under /arxiv — PDFs, LaTeX source, metadata, versions, and per-category recent submissions.
---

The arXiv provider mounts under `/arxiv`. Each paper is a directory; its
metadata, PDF, and source are files. The same per-paper subtree appears
wherever a paper shows up — under `papers/`, a category's recent pages, or a
submission-day bucket — so the file shape is identical no matter how you got there.

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
| `/arxiv/categories/{cat}/recent/` | Recent submissions in `cat` (`fetched`, `pages`, `status.json`) |
| `/arxiv/categories/{cat}/recent/pages/{n}/` | One feed page of recent submissions (`start = n × 100`) |
| `/arxiv/categories/{cat}/recent/fetched/` | Papers already fetched and cached for `cat` |
| `/arxiv/categories/{cat}/submissions/{YYYYMMDD}/` | Papers submitted on that UTC day |

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

## Browsing a category

A category is browsed through a moving **recent** scan and the immutable
**submission-day** buckets derived from it. Each entry lists papers you can
descend into — the per-paper subtree is the same as under `/arxiv/papers/{id}/`.

```bash
# Recent submissions in a subject category
ls /arxiv/categories/cs.AI/recent

# Fetch one feed page at a time (start = n × 100)
ls /arxiv/categories/cs.AI/recent/pages/1

# Papers already fetched and cached for the category
ls /arxiv/categories/cs.AI/recent/fetched

# Scan progress: feed snapshot, totals, next page to fetch
cat /arxiv/categories/cs.AI/recent/status.json | jq

# Papers submitted on a discovered UTC day
ls /arxiv/categories/cs.AI/submissions
ls /arxiv/categories/cs.AI/submissions/20260512
```

:::note
Listing `recent/pages/{n}` is the explicit "fetch next page" action. Submission-day
buckets read only from already-fetched state, so a day appears once a page that
contains it has been scanned.
:::

:::tip
Combine arXiv with the inspection tools — `jq` reads the metadata files
directly, so you can extract just the fields you need:

```bash
jq -r '.title' /arxiv/papers/1706.03762/metadata.json
```
:::
