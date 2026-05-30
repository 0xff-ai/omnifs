---
title: Project Everything You Fetched
description: Return every byte and child a payload already carries — siblings via with_siblings, nested children via Effects — to avoid refetches.
---

The single most important performance rule for a provider: **if a handler has an upstream payload in hand, project everything that payload can produce.** A user who lists a directory and then reads three files in it should cause one upstream fetch, not four. Returning only the requested field and forcing later refetches is wrong.

There are two channels for this, depending on how the extra content relates to the looked-up target.

## Siblings: `Lookup::with_siblings`

When a `lookup_child` returns one entry but the same payload also describes its **siblings** in the same directory, attach them with `with_siblings`. The host caches them alongside the looked-up target, so a later stat or read of a sibling avoids a round trip.

```rust
// One API call returned the whole issue; project every sibling file it implies.
let target = Entry::file("body.md", FileProj::inline(issue.body.into_bytes(), Stability::Mutable, None));
let siblings = vec![
    Entry::file("title.txt", FileProj::inline(issue.title.into_bytes(), Stability::Mutable, None)),
    Entry::file("meta.json", FileProj::inline(serde_json::to_vec_pretty(&issue.meta)?, Stability::Mutable, None)),
];
Ok(Lookup::entry(target).with_siblings(siblings))
```

By default a lookup with siblings is exhaustive: the host treats absence from the sibling set as an authoritative negative. Call `.exhaustive(false)` if there are siblings you did not enumerate.

## Adjacent file content from a read: `FileContent::with_effects`

When a `read_file` materializes a payload that also contains the bytes of **adjacent projected files**, stage them so the host caches them too. `FileContent` carries an `Effects` batch; use `Effects::project_file` to install each adjacent file's projection.

```rust
#[file("{category}/{paper_id}/abstract.txt")]
fn abstract_file(category: &str, paper_id: &str, cx: &Cx) -> Result<FileContent> {
    let entry = first_entry(cx, paper_id)?; // one fetch returns the whole paper

    let mut effects = Effects::new();
    // The same payload carries title.txt and meta.json — project them now.
    effects.project_file(
        format!("{category}/{paper_id}/title.txt"),
        FileProj::inline(entry.title.clone().into_bytes(), Stability::Immutable, None),
    )?;
    effects.project_file(
        format!("{category}/{paper_id}/meta.json"),
        FileProj::inline(serde_json::to_vec_pretty(&entry.as_meta())?, Stability::Immutable, None),
    )?;

    Ok(FileContent::new(entry.summary.into_bytes()).with_effects(effects))
}
```

## Nested children from a listing: `Effects` projection

When a `list_children` payload carries the contents of **nested children** (files inside the listed directory, or grandchildren), stage them as projection effects on the `List`. Use `Effects::project_file` for files and `Effects::project_dir` / `project_dir_exhaustive` for directories, then attach the batch with `with_effects`.

```rust
#[dir("{owner}/{repo}")]
fn repo_dir(owner: &str, repo: &str, cx: &Cx) -> Result<List> {
    let meta = fetch_repo(cx, owner, repo)?; // one fetch returns repo metadata

    let mut effects = Effects::new();
    // The metadata payload already contains meta.json's bytes — cache them.
    effects.project_file(
        format!("{owner}/{repo}/meta.json"),
        FileProj::inline(serde_json::to_vec_pretty(&meta)?, Stability::Mutable, None),
    )?;

    let listing = Listing::partial([
        Entry::file("meta.json", FileProj::inline(serde_json::to_vec_pretty(&meta)?, Stability::Mutable, None)),
        Entry::dir("tree"),
    ]);
    Ok(List::entries(listing).with_effects(effects))
}
```

When you project a directory whose children appear in the same effects batch and you have listed all of them, use `project_dir_exhaustive` so the host marks that listing authoritative and serves a later `readdir` from cache without re-invoking `list_children`.

## Choosing the channel

```mermaid
flowchart TD
    P["a handler holds an upstream payload"] --> Q{"what does it also contain?"}
    Q -->|siblings of a looked-up entry| A["Lookup::with_siblings(..)"]
    Q -->|adjacent files of a read file| B["FileContent::with_effects(Effects::project_file)"]
    Q -->|nested children of a listed dir| C["List::with_effects(Effects::project_file / project_dir)"]
```

## Why this matters

External services return rich payloads: one GitHub repo call carries name, description, default branch, and counts; one arXiv entry carries title, abstract, authors, and PDF link. If you discard everything but the one field the current path asked for, every sibling read becomes another upstream call — slower for the user and harder on rate limits. The host caches whatever you project, so projecting the full payload turns N reads into one fetch.

:::tip
Effect paths are provider-relative and must be normalized: no empty, `.`, or `..` segments. `project_file` validates the projection (including the Volatile-requires-Ranged rule) before staging it, returning a `ProviderError` you can propagate with `?`.
:::
