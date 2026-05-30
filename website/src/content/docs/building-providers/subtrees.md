---
title: Subtrees
description: Typed #[subtree] dispatch via #[bind] versus #[treeref] clone handoff, and when to use each.
---

omnifs has two distinct mechanisms whose names both involve "subtree". They solve different problems. Get the distinction right before reaching for either.

| Mechanism | Attribute | What the host does |
| --- | --- | --- |
| Clone / archive handoff | `#[treeref("...")]` | Bind-mounts a real on-disk tree (git clone, extracted archive) at the path |
| Typed subtree dispatch | `#[bind("...")]` + `#[omnifs_sdk::subtree]` | Routes the path suffix through a typed Rust value's own handlers |

## Typed subtrees (`#[subtree]` + `#[bind]`)

A typed subtree groups all routes that share a parsed prefix into one Rust type. A `#[bind]` handler parses the prefix captures and constructs the value; a `#[omnifs_sdk::subtree] impl` block holds the inner handlers, whose patterns are **relative to the subtree root** and whose methods take `&self`.

```rust
// tables.rs — parse the {name} prefix, validate, build a Table.
#[omnifs_sdk::handlers(state = State)]
impl Tables {
    #[dir("/tables")]
    async fn tables(cx: Cx<State>) -> Result<Listing> {
        let names = cx.state(|s| s.backend.borrow_mut().table_names())?;
        Ok(Listing::complete(names.into_iter().map(Entry::dir).collect::<Vec<_>>()))
    }

    #[bind("/tables/{name}")]
    async fn table(cx: Cx<State>, name: String) -> Result<Table> {
        let exists = cx.state(|s| s.backend.borrow_mut().table_exists(&name))?;
        if !exists {
            return Err(ProviderError::not_found(format!("no such table: {name}")));
        }
        Ok(Table::new(name))
    }
}
```

```rust
// table_subtree.rs — everything under one table. Patterns are relative
// to /tables/{name}. Methods take &self.
pub(crate) struct Table { name: String }

impl Table {
    pub fn new(name: String) -> Self { Self { name } }
}

#[subtree]
impl Table {
    #[dir("/")]
    async fn files(&self, cx: Cx<State>) -> Result<Listing> {
        Ok(Listing::complete(vec![
            Entry::file("schema.sql", deferred()),
            Entry::file("count.txt", deferred()),
            Entry::file("sample.json", deferred()),
        ]))
    }

    #[file("/schema.sql")]
    async fn schema(&self, cx: Cx<State>) -> Result<FileContent> {
        let sql = cx.state(|s| s.backend.borrow_mut().table_schema(&self.name))?;
        Ok(FileContent::new(sql))
    }
}
```

Inner handlers read the parsed prefix from `&self` (`self.name`) without re-parsing it on each route. The empty pattern `#[dir("/")]` is the subtree root — what the user sees when they `ls` the bound directory.

The `#[subtree]` attribute accepts an optional `state =` argument when the impl's handlers need a different state type than the default; most subtrees inherit the provider state through `Cx<State>` in their handler signatures.

### How dispatch flows

For a request to `/tables/Track/schema.sql`:

```mermaid
flowchart TD
    R["list/lookup/read for /tables/Track/schema.sql"] --> B["#[bind(\"/tables/{name}\")] matches 'Track'"]
    B --> C["construct Table { name: 'Track' } (validates existence)"]
    C --> I["dispatch suffix '/schema.sql' through Table's inner registry"]
    I --> F["#[file(\"/schema.sql\")] on Table -> schema(&self, cx)"]
```

The host parses the prefix once, builds the value once, then runs the suffix through the inner route table using the same precedence rules as the top level. A `#[bind]` that returns `not-found` (like a missing table) fails fast at bind time.

## Clone / archive handoff (`#[treeref]`)

Use `#[treeref]` when the data behind a path is a genuine directory tree the host can materialize: a git repository or an extractable archive. Instead of projecting every file yourself, you hand the host a `TreeRef` and it bind-mounts the real contents.

```rust
#[treeref("/{owner}/{repo}/repo")]
async fn repo_tree(cx: Cx<State>, owner: String, repo: String) -> Result<TreeRef> {
    let repo_id = repo::ensure_repo(&cx, &owner, &repo).await?;
    repo::open_tree(&cx, repo_id).await?;       // git-open-repo callout
    Ok(TreeRef::new(repo_id.raw()))
}
```

The terminal is a `subtree(tree-ref)` result variant; the SDK also stages the host-side install so the bind mount appears at that path. From then on, reads under `repo/` are served by the host from the materialized clone, not by your provider.

You get a `TreeRef` from `cx.git().open(clone_url, cache_key).await?` for repositories, or from `cx.archives().open(blob, format, strip_prefix).await?` for a stored archive blob.

## Choosing between them

- The path is backed by a real cloneable/extractable tree → `#[treeref]`. You write nothing per-file; the host serves the bytes.
- The path is a logical grouping you project yourself, but every route shares a parsed, validated prefix → `#[subtree]` + `#[bind]`. You still project each file, but the prefix is parsed once and shared via `&self`.
- The prefix is trivial and routes do not share state → just use top-level `#[dir]`/`#[file]` with captures.

:::note
The WIT keeps the arm name `subtree` on the `lookup-result`/`list-result` variants for the clone handoff. The SDK attribute for that handoff is `#[treeref]`; `#[subtree]` is reserved for typed-subtree-dispatch impl blocks. Two names, two mechanisms.
:::
