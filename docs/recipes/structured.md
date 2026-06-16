---
title: Inspect structured data
description: Query projected JSON and structured leaves with jq and similar tools.
---

# Inspect structured data

**Query JSON with jq**

    jq '.title' /omnifs/arxiv/papers/2301.00001/v1/json

The `.json` leaf is real JSON with the right content type, so `jq` works unmodified.

**Filter a listing**

    jq '.[] | select(.State=="running") | .Names' /omnifs/docker/containers.json

The Docker provider projects daemon state as JSON files, and `jq` does the rest.

**Pull one field from a sample**

    jq '.[0]' /omnifs/db/tables/artists/sample.json

Structured tools work because the provider declares accurate content types, not because omnifs ships a query language. You name the path, and `jq` filters the result.
