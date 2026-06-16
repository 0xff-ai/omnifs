---
title: Mount GitHub and inspect issues
description: Authenticate GitHub and read an issue as a set of files to make the object model concrete.
---

# Mount GitHub and inspect issues

Goal: authenticate GitHub and read an issue as a set of files, so the object model is concrete.

1. `omnifs init github`
2. Complete the device flow it prints, or pass a token; init reuses `gh auth token` if you let it.
3. `omnifs up`
4. `omnifs shell`
5. `cat /omnifs/github/rust-lang/rust/issues/open/12345/title`
6. `cat /omnifs/github/rust-lang/rust/issues/open/12345/body`
7. `ls /omnifs/github/rust-lang/rust/issues/open/12345/comments`

## Result

You read `title`, `body`, and `state` as separate files, but the provider fetched the issue once and projected all of them from that one payload, so steps 5 through 7 cost a single upstream call. The `item.md` leaf in the same directory is the issue rendered as Markdown. Read it again and nothing goes upstream, because it comes from the object cache. The object flavour works exactly this way: the provider stored one payload, and the files are all views of it.
