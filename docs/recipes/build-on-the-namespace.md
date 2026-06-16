---
title: Build on the namespace (for app developers)
description: Read projected paths from application code without writing auth, caching, or API clients.
---

# Build on the namespace (for app developers)

**Read paths instead of integrating**

An app that wants GitHub, Linear, and database data reads the projected paths. It needs no OAuth client, API wrapper, or cache layer, because the host provides those for every consumer that reads the path.

**Read paths, build a view**

    open("/omnifs/github/rust-lang/rust/issues/open/12345/title").read()

Your app reads a file. The provider, the auth, the caching, and the freshness are someone else's concern.

**Detect, request, degrade**

Detect the mount before relying on it (is `/omnifs` present and ready), request the scope your app needs (a worldview, when that ships; today, the mounts it expects), and degrade gracefully when a mount or provider is absent rather than failing hard. Worldview is roadmap vocabulary. Until it lands, an app depends on the mounts the user configured and checks for them.
