---
title: Subtrees
description: Typed #[subtree] dispatch via #[bind] versus #[treeref] clone handoff, and when to use each.
---

omnifs has two distinct mechanisms whose names both contain "subtree". They solve different problems. Get the distinction right before you reach for either.

| Mechanism | Attribute | What the host does |
| --- | --- | --- |
| Clone / archive handoff | `#[treeref("...")]` | Bind-mounts a real on-disk tree (git clone, extracted archive) at the path |
| Typed subtree dispatch | `#[bind("...")]` + `#[omnifs_sdk::subtree]` | Routes the path suffix through a typed Rust value's own handlers |

## Typed subtrees (`#[subtree]` + `#[bind]`)

A typed subtree groups all routes that share a parsed prefix into one Rust type. The `#[bind]` handler parses the prefix captures and constructs the value; the `#[omnifs_sdk::subtree] impl` block holds the inner handlers, whose patterns are **relative to the subtree root**.

```rust
#[omnifs_sdk::handlers]
impl DbProvider {
    #[dir("")]
    fn root(cx: &Cx) -> Result<List> {
        let cfg = cx.config::<DbConfig>()?;
        Ok(List::entries(Listing::complete(cfg.databases.keys().map(Entry::dir))))
    }

    // Parse the {database} prefix, build a Database, hand off the rest.
    #[bind("{database}")]
    fn database(database: &str, cx: &Cx) -> Result<Database> {
        let path = cx.config::<DbConfig>()?
            .databases.get(database).cloned()
            .ok_or_else(|| ProviderError::not_found("no such database"))?;
        Ok(Database { name: database.into(), path })
    }
}

/// Everything under one database. Inner patterns are relative to {database}.
#[omnifs_sdk::subtree]
impl Database {
    #[dir("")]
    fn tables(&self, cx: &Cx) -> Result<List> {
        let conn = self.open(cx)?;
        Ok(List::entries(Listing::complete(list_tables(&conn)?.iter().map(Entry::dir))))
    }

    #[dir("{table}")]
    fn table_dir(&self, table: &str, cx: &Cx) -> Result<List> {
        let files = vec![
            Entry::file("rows.json", FileProj::deferred_full(Size::NonZero, Stability::Mutable, None)),
            Entry::file("schema.sql", FileProj::deferred_full(Size::NonZero, Stability::Mutable, None)),
        ];
        Ok(List::entries(Listing::complete(files)))
    }

    #[file("{table}/{filename}")]
    fn table_file(&self, table: &str, filename: &str, cx: &Cx) -> Result<FileContent> {
        let conn = self.open(cx)?;
        Ok(FileContent::new(render(&conn, table, filename)?))
    }
}
```

Inner handlers take `&self`, so the parsed prefix data (`self.name`, `self.path`) is available to every route in the subtree without re-parsing. The empty pattern `#[dir("")]` is the subtree root — what the user sees when they `ls` the bound directory.

### How dispatch flows

For a request to `chinook/Track/rows.json`:

```mermaid
flowchart TD
    R["list/lookup/read for chinook/Track/rows.json"] --> B["#[bind(\"{database}\")] matches 'chinook'"]
    B --> C["construct Database { name: 'chinook', path }"]
    C --> I["dispatch suffix 'Track/rows.json' through Database's inner registry"]
    I --> F["#[file(\"{table}/{filename}\")] on Database -> table_file(self, 'Track', 'rows.json')"]
```

The host parses the prefix once, builds the value once, then runs the suffix through the inner route table using the same precedence rules as the top level.

## Clone / archive handoff (`#[treeref]`)

Use `#[treeref]` when the data behind a path is a genuine directory tree that already exists somewhere the host can materialize: a git repository or an extractable archive. Instead of projecting every file yourself, you hand the host a `tree` handle and it bind-mounts the real contents.

```rust
#[treeref("{owner}/{repo}/tree")]
fn repo_tree(owner: &str, repo: &str, cx: &Cx) -> Result<List> {
    let tree = cx.git_open(
        format!("git@github.com:{owner}/{repo}.git"),
        format!("github-{owner}-{repo}"),
    )?;
    Ok(List::subtree(format!("{owner}/{repo}/tree"), tree))
}
```

Under the hood the terminal is a `subtree(tree-ref)` result variant; the SDK also stages a `disown-tree` effect so the host installs the bind mount at the response boundary. From that point on, reads under `tree/` are served by the host from the materialized clone, not by your provider.

## Choosing between them

- The path is backed by a real cloneable/extractable tree → `#[treeref]`. You write nothing per-file; the host serves the bytes.
- The path is a logical grouping you project yourself, but every route shares a parsed prefix → `#[subtree]` + `#[bind]`. You still project each file, but the prefix is parsed once and shared.
- The prefix is trivial and routes do not share state → just use top-level `#[dir]`/`#[file]` with captures.

:::note
The WIT keeps the arm name `subtree` on the `lookup-result` and `list-result` variants for the clone handoff. The SDK attribute for that handoff is `#[treeref]`; `#[subtree]` is reserved for typed-subtree-dispatch impl blocks. Two names, two mechanisms.
:::
