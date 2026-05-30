---
title: Handlers
description: The handler attributes — dir, file, treeref, bind, mutate — and how a path routes to a handler.
---

Handlers are `async fn`s inside a `#[omnifs_sdk::handlers(state = State)] impl` block. Each carries a path-pattern attribute. The SDK builds a route table from those patterns and dispatches each browse call to the most specific matching handler.

## Handler shape

Every handler is `async`, takes `cx: Cx<State>` as the first parameter, then one parameter per captured path segment, and returns a `Result<T>` whose `T` depends on the attribute.

```rust
#[omnifs_sdk::handlers(state = State)]
impl Issues {
    #[dir("/{owner}/{repo}/issues")]
    async fn issues_dir(cx: Cx<State>, owner: String, repo: String) -> Result<Listing> {
        let issues = cx.api_get_json::<Vec<Issue>>(&format!("/repos/{owner}/{repo}/issues")).await?;
        let entries = issues.iter().map(|i| Entry::dir(i.number.to_string())).collect::<Vec<_>>();
        Ok(Listing::partial(entries))
    }
}
```

The `impl Issues` name is just a grouping label; routes are registered by pattern, not by impl name.

## Path patterns and captures

A pattern is a slash-prefixed, slash-separated template. A literal segment matches itself; a `{name}` segment captures one path component and is passed to the handler as a parameter of the same name. Capture parameters are **typed**: use `String` for a free segment, or a custom type that implements the SDK's segment-parse trait to validate the segment (for example `DomainName`, `PaperId`, `SupportedRecordType`). A type that rejects a value removes that route from candidacy (see below).

```rust
#[file("/{domain}/{record_type}")]
async fn record_file(cx: Cx<State>, domain: DomainName, record_type: SupportedRecordType)
    -> Result<FileContent> { /* ... */ }
```

Parameter order in the signature follows segment order in the pattern.

## The attributes

### `#[dir("...")]` → `Result<Listing>`

A directory family. Returns the listing of children at that path.

```rust
#[dir("/tables")]
async fn tables(cx: Cx<State>) -> Result<Listing> {
    let names = cx.state(|s| s.backend.borrow_mut().table_names())?;
    Ok(Listing::complete(names.into_iter().map(Entry::dir).collect::<Vec<_>>()))
}
```

`Listing::complete(..)` is an exhaustive listing (the host treats absence as authoritative negative). `Listing::partial(..)` is non-exhaustive — use it for an open namespace whose members are resolved on demand, like a DNS root or a paged feed.

### `#[file("...")]` → `Result<FileContent>`

An exact file family. Returns the bytes for a `read_file`.

```rust
#[file("/{domain}/{record_type}")]
async fn record_file(cx: Cx<State>, domain: DomainName, record_type: SupportedRecordType)
    -> Result<FileContent> {
    let records = doh::resolve(&cx, &domain, record_type).await?;
    Ok(FileContent::new(render_records(&records)))
}
```

### `#[treeref("...")]` → `Result<TreeRef>`

A subtree handoff: the matched path is a real directory tree the host should materialize from a clone or archive rather than projecting file-by-file. The handler obtains a `TreeRef` from a callout (`cx.git().open(..)` or `cx.archives().open(..)`) and returns it. The host bind-mounts the resolved tree at that path.

```rust
#[treeref("/{owner}/{repo}/repo")]
async fn repo_tree(cx: Cx<State>, owner: String, repo: String) -> Result<TreeRef> {
    let repo_id = repo::ensure_repo(&cx, &owner, &repo).await?;
    repo::open_tree(&cx, repo_id).await?;
    Ok(TreeRef::new(repo_id.raw()))
}
```

### `#[bind("...")]` → `Result<SubtreeType>`

Mounts a typed subtree at this path family. The handler parses the prefix captures, validates, and returns a value of a `#[omnifs_sdk::subtree] impl` type. The host dispatches the remaining suffix through that type's own handlers. See [Subtrees](./subtrees/).

```rust
#[bind("/tables/{name}")]
async fn table(cx: Cx<State>, name: String) -> Result<Table> {
    let exists = cx.state(|s| s.backend.borrow_mut().table_exists(&name))?;
    if !exists {
        return Err(ProviderError::not_found(format!("no such table: {name}")));
    }
    Ok(Table::new(name))
}
```

### `#[mutate("...")]`

A mutation handler family.

:::caution
Mutations are not implemented yet. Do not make projected files writable as an implicit mutation mechanism. If you are adding mutation support, follow the draft-namespace + control-directory design in the project guidance, not direct writes.
:::

## Auto-navigable prefixes

You do **not** write stub `#[dir]` handlers for intermediate navigation nodes. Any literal-segment prefix of a registered route is automatically a navigable directory. If your routes are `#[dir("/{owner}/{repo}/issues")]` and friends, the paths `/{owner}` and `/{owner}/{repo}` are still listable and `cd`-able even though no handler is bound to them. Adding empty pass-through handlers for these is wrong. (A root `#[dir("/")]` returning `Listing::partial(vec![])` is the one common exception, used to present a valid but open root directory.)

## Per-segment validators and match candidacy

Typed capture parsers participate in match candidacy. A capture parameter whose type rejects the segment removes that route from the candidate set; dispatch then **falls through to the next-most-specific candidate**, not straight to `ENOENT`. This is how a literal route like `/{owner}/{repo}/repo` can coexist with a dynamic sibling capture: the literal wins for `repo`, the capture handles everything else, and a malformed capture falls through rather than masking a valid route.

## How a path routes

```mermaid
flowchart TD
    A["browse call for a path"] --> B{"#[treeref] / subtree match?"}
    B -->|yes| S["return TreeRef / subtree terminal"]
    B -->|no| C{"exact #[file] / #[dir] / static shape?"}
    C -->|yes| H["dispatch to that handler"]
    C -->|no| D{"auto-navigable literal prefix?"}
    D -->|yes| N["synthesize navigable directory"]
    D -->|no| E{"parent #[dir] handler covers dynamic child?"}
    E -->|yes| P["dispatch to parent #[dir]"]
    E -->|no| F["not-found"]
```

The precedence, in words: subtree/treeref handlers first, then exact / static / auto-navigable shape, then the parent `#[dir]` handler for dynamic children, then not-found. A rejected per-segment validator removes a candidate but does not short-circuit the search.

:::note
`docs/design/path-dispatch-and-listing.md` in the repo is the source of truth for routing precedence and listing exhaustiveness. The summary here matches it; read that file before changing dispatch logic itself.
:::
