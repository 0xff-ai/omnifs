---
title: Add object and view caching
description: Turn a path-oriented route into an object so its canonical bytes cache durably and its leaves re-render for free.
---

# Add object and view caching

Goal: turn a path-oriented route into an object so its canonical bytes cache durably and its leaves re-render for free.

Mark the resource type as an object and give its key a `load`:

```rust
#[omnifs_sdk::object(kind = "example.item", key = ItemKey)]
#[derive(serde::Serialize, serde::Deserialize)]
struct Item { id: u64, name: String, body: String }

impl Key for ItemKey {
    type Object = Item;
    type State = State;
    async fn load(&self, cx: &Cx<State>, since: Option<Validator>) -> Result<Load<Item>> {
        cx.endpoint::<ExampleApi>()
            .get(format!("/v1/items/{}", self.id))
            .maybe_if_none_match(since.as_ref())
            .load::<Item>()
            .await
    }
}
```

Register it as an object route and declare its leaves:

```rust
r.object::<Item>("/items/{id}", |o| {
    o.representations("item", (Markdown,))?;
    o.file("name").project(Item::name)?;
    o.file("body").project(Item::body)?;
    Ok(())
})?;
```

## Result

When `load` returns `Load::Fresh`, the SDK stores the canonical item bytes in the object cache and records that `name`, `body`, `item.json`, and `item.md` map to it. Read `name`, then `body`: the second read renders from the cached canonical with no upstream call. A `304` from `maybe_if_none_match` returns `Load::Unchanged`, so revalidation is cheap. You added durable caching by describing the resource, not by writing a cache.
