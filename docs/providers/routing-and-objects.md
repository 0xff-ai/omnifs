---
title: Routing and objects
description: How provider authors register routes, parse captures, define objects, and expose field leaves and rendered representations.
---

Provider routes are registered imperatively in `start`.

```rust
fn start(config: Config, r: &mut Router<State>) -> Result<State> {
    let state = State::open(config)?;

    r.dir("/tables").handler(tables_list)?;
    r.file("/tables/{table}/schema.sql").handler(table_schema_sql)?;

    Ok(state)
}
```

There are no `#[dir]`, `#[file]`, or `#[treeref]` route attributes.

## Route methods

| Method | Use |
|---|---|
| `r.dir(template).handler(handler)` | Directory listing and lookup behavior. |
| `r.file(template).handler(handler)` | File reads. |
| `r.treeref(template).handler(handler)` | Host handoff to a real backing tree. |
| `r.object::<O>(template, block)` | Directory-shaped object with representations and leaves. |
| `r.file_object::<O>(template, block)` | File-shaped object. |
| `r.attach(prefix, &handle)` | Attach a detached object subtree at a prefix. |

Templates are absolute provider-relative paths. Variable segments use `{name}` captures.

## Capture parsing

Capture types parse typed path segments. If a capture parser rejects a segment, that route is not a candidate. This lets a provider express path shape without manually validating every handler.

GitHub issue numbers and Linear issue identifiers are not just strings. They are typed captures that decide whether a route can own a path.

## Object routes

Use an object route when several files describe the same upstream resource.

```rust
r.object::<Issue>("/{owner}/{repo}/issues/{filter}/{number}", |o| {
    o.representations("item", (Markdown,))?;
    o.file("title").project(Issue::title)?;
    o.file("body").lazy().project(Issue::body)?;
    o.file("state").project(Issue::state)?;
    Ok(())
})?;
```

The object route creates representation files such as `item.json` and `item.md`, plus field leaves like `title` and `state`. The object type still needs a key and `Key::load` implementation. That load function defines object identity, fetch behavior, canonical bytes, validators, and not-found behavior.

## Directory listings

A directory listing may be exhaustive or open. Exhaustive means the provider is declaring these are the names it knows at that path. Open means a later direct lookup may resolve names that were not listed.

Use open listings for API-backed collections where the provider can look up a specific resource by id even when the listing is paged or truncated.
