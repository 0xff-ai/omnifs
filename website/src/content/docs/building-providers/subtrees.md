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

A typed subtree groups all routes that share a parsed prefix into one Rust type. A `#[bind]` handler parses and validates the prefix captures and constructs the value; a `#[omnifs_sdk::subtree] impl` block holds the inner handlers, whose patterns are **relative to the subtree root** and whose context is `&BindCtx<'_, State, B>`. The parsed prefix is read back through `cx.bindings()`.

```rust
// tables.rs — parse the {name} prefix, build a TableSubtree.
pub struct TableHandlers;

#[handlers]
impl TableHandlers {
    #[dir("/tables")]
    fn list(cx: &DirCx<State>) -> Result<Projection> {
        let names = cx.state(|s| s.backend.borrow().list_tables())
            .map_err(|e| ProviderError::internal(format!("list tables: {e}")))?;
        let mut p = Projection::new();
        for name in names { p.dir(name); }
        p.page(PageStatus::Exhaustive);
        Ok(p)
    }

    #[bind("/tables/{name}")]
    fn table(_cx: &Cx<State>, name: TableName) -> Result<TableSubtree> {
        Ok(TableSubtree { name: name.into_inner() })
    }
}
```

```rust
// table_subtree.rs — everything under one table. Patterns are relative
// to /tables/{name}. Context is BindCtx; the prefix is in cx.bindings().
pub struct TableSubtree {
    pub name: String,
}

#[subtree]
impl TableSubtree {
    #[dir("/")]
    fn root(cx: &BindCtx<'_, State, TableSubtree>) -> Result<Projection> {
        ensure_table_exists(cx)?;
        // Sibling #[file] handlers project their own entries; marking the
        // listing exhaustive lets the host satisfy negative lookups locally.
        let mut p = Projection::new();
        p.page(PageStatus::Exhaustive);
        Ok(p)
    }

    #[file("/schema.sql")]
    fn schema_sql(cx: &BindCtx<'_, State, TableSubtree>) -> Result<FileContent> {
        let table = cx.bindings().name.clone();
        let sql = cx.state(|s| s.backend.borrow().table_create_sql(&table))
            .map_err(|e| ProviderError::internal(format!("read schema: {e}")))?
            .ok_or_else(|| ProviderError::not_found(format!("table not found: {table}")))?;
        Ok(FileContent::bytes(sql))
    }
}
```

`cx.bindings()` returns `&TableSubtree`, so `self.name` data is available to every route without re-parsing. `BindCtx` derefs to `Cx<State>`, so `cx.http()`, `cx.git()`, and `cx.state()` work directly. The empty pattern `#[dir("/")]` is the subtree root — what the user sees when they `ls` the bound directory.

### How dispatch flows

For a request to `/tables/Track/schema.sql`:

```mermaid
flowchart TD
    R["list/lookup/read for /tables/Track/schema.sql"] --> B["#[bind(\"/tables/{name}\")] matches 'Track'"]
    B --> C["construct TableSubtree { name: 'Track' }"]
    C --> I["dispatch suffix '/schema.sql' through TableSubtree's inner registry"]
    I --> F["#[file(\"/schema.sql\")] on TableSubtree -> schema_sql(cx)"]
```

The host parses the prefix once, builds the value once, then runs the suffix through the inner route table using the same precedence rules as the top level. A `#[bind]` handler can validate cheaply (a `FromStr` on the capture) and defer existence checks to the inner handlers, or return `not-found` at bind time.

## Clone / archive handoff (`#[treeref]`)

Use `#[treeref]` when the data behind a path is a genuine directory tree the host can materialize: a git repository or an extractable archive. Instead of projecting every file yourself, you hand the host a `TreeRef` and it bind-mounts the real contents.

```rust
#[treeref("/{owner}/{repo}/repo")]
async fn repo_tree(cx: &Cx<State>, owner: OwnerName, repo: RepoName) -> Result<TreeRef> {
    let repo_id = RepoId::new(&owner, &repo);
    let repo = cx.git()
        .open_repo(
            format!("github.com/{repo_id}"),          // cache key
            format!("git@github.com:{repo_id}.git"),  // clone URL
        )
        .await?;                                       // git-open-repo callout
    Ok(TreeRef::new(repo.tree))
}
```

`cx.git().open_repo(cache_key, clone_url)` returns a `GitRepoInfo`; its `.tree` field is the `tree-ref` you wrap in `TreeRef::new`. For a stored archive blob, use `cx.archives().open(blob).format(ArchiveFormat::TarGz).strip_prefix("foo/").send().await?`, which returns a `TreeRef` directly. From then on, reads under that path are served by the host from the materialized tree, not by your provider.

## Choosing between them

- The path is backed by a real cloneable/extractable tree → `#[treeref]`. You write nothing per-file; the host serves the bytes.
- The path is a logical grouping you project yourself, but every route shares a parsed, validated prefix → `#[subtree]` + `#[bind]`. You still project each file, but the prefix is parsed once and read via `cx.bindings()`.
- The prefix is trivial and routes do not share state → just use top-level `#[dir]`/`#[file]` with captures.

:::note
The WIT keeps the arm name `subtree` on the `lookup-result`/`list-result` variants for the clone handoff. The SDK attribute for that handoff is `#[treeref]`; `#[subtree]` is reserved for typed-subtree-dispatch impl blocks. Two names, two mechanisms.
:::
