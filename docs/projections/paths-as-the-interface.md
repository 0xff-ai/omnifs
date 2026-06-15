---
title: Paths as the interface
description: Why omnifs uses the filesystem path as its one universal address for every resource.
---

In omnifs you address a resource by its path. `/omnifs/github/rust-lang/rust/issues/open/12345/title` is a complete request: the provider, the resource, the field. There is no client object to construct and no response schema to parse. You name where the thing lives, and that is the call.

This works because the filesystem is the one interface with a fifty-year compatibility guarantee in both directions. Backward, every tool ever written speaks it: `cat`, `grep`, `find`, `make`, `rsync`, every editor, every shell. Forward, every agent trained on shell transcripts and repositories already knows how to navigate trees and read files. Nobody has to learn paths for omnifs. They are the surface everything already targets.

A path is also a stable handle. You can bookmark it, log it, put it in a Makefile, or pass it between a human and an agent, and it still means the same thing. That stability lets a query live at a path and its result be read as a file.
