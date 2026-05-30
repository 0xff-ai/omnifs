---
title: FAQ
description: Frequently asked questions about omnifs — Docker, macOS visibility, tokens, SSH cloning, callouts, caching, and write-back.
---

## Why does omnifs need Docker or a container?

The runtime FUSE mount is Linux-only. The CLI runs natively on macOS and Linux, but it
talks to a Linux container that holds the actual mount. On macOS the mount lives inside the
Docker Desktop Linux VM; on Linux it runs in a Docker container too. `omnifs up` pulls and
starts the matching runtime image, then `omnifs shell` drops you into it.

## Why isn't the mount visible in Finder on macOS?

Because the mount lives inside the Linux container, not on the macOS host. On macOS you
reach it through `omnifs shell`, not as a native Finder volume or host-shell path. There is
no macFUSE or `diskutil` involvement. Native macOS and Windows mounts are
[planned](/reference/roadmap/), not shipped.

## Are my GitHub tokens read-only?

The bundled GitHub provider uses device-code OAuth with the bundled public client id and
**no default write scopes**. Linear uses browser PKCE OAuth with `read` scope. Browsing is
read-only today; write-back is a [work in progress](/reference/future/).

## Why does cloning use SSH instead of HTTPS?

Git clone currently uses SSH. The remote form is `git@github.com:<owner>/<repo>.git` and
auth comes from your forwarded `SSH_AUTH_SOCK`. Your private key is never copied into the
container — the container only asks your running SSH agent to sign while the socket is
mounted. Switching the clone transport to HTTPS/token would change the operational
contract, so it would be called out explicitly if it ever happened.

To check your setup:

```bash
echo "$SSH_AUTH_SOCK"
ssh-add -L
ssh -T git@github.com
```

## Do providers touch the network directly?

No. Providers never make network or Git calls themselves. They emit **callouts** —
descriptions of the work they need ("fetch this endpoint", "clone this repo") — and the
host executes them. This keeps providers sandboxed and lets the host own caching, rate
limits, auth injection, and concurrency. See the [glossary](/reference/glossary/).

## Is there a TTL on the cache?

No. The host owns all caching with capacity-bounded caches and **no TTLs**. Entries leave
the cache only by capacity eviction or by explicit invalidation — from `event-outcome`
fields returned by `on-event` handlers, or from the FUSE notifier. Providers must not add
their own LRUs or time-based expiration.

## Can I write back yet?

Not in a stable form. Write-back is a work in progress and is designed around Git: edit in
the mounted scope, then `git add`, `git commit`, and `git push` to reconcile changes
through the provider. A raw write syscall alone never triggers a remote mutation. See the
[future design](/reference/future/).

## Where are my credentials and configs stored?

On the host:

- **Mount configs**: `~/.omnifs/config/mounts/<name>.json`.
- **Credentials**: in the OS keychain, or a file fallback at
  `~/.omnifs/data/credentials.json`. One backend is chosen at startup.

For the contributor sandbox, `omnifs dev` instead captures your `gh auth token` and exposes
it as a read-only mounted secret file inside the container.

## Is Windows supported?

Not yet. The runtime mount is Linux-only today. macOS works through the Docker Desktop
Linux container, and native macOS and Windows support are [planned](/reference/roadmap/).
