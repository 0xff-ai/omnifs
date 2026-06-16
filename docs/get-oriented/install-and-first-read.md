---
title: Install and first read
description: Install omnifs, configure a provider, start the runtime, and read your first service paths.
---

## Install

omnifs is distributed as an npm package. Node 18 or later is required.

```bash
npm install -g @0xff-ai/omnifs
omnifs --version
```

The FUSE surface is Linux. On macOS, omnifs runs the Linux runtime in a container and you read paths from `omnifs shell`. Windows support and native non-Linux surfaces are outside the supported runtime path.

## Configure a provider

The guided setup is the fastest path:

```bash
omnifs setup
```

You can also configure one provider at a time:

```bash
omnifs init dns
omnifs init github
```

Providers that need credentials prompt for them. Credential and local-resource requirements vary by provider; use the [provider catalogue](/providers/) as the source of truth.

## Start the runtime

```bash
omnifs up
omnifs shell
```

Inside the shell, the projection root is `/omnifs`. Each configured provider appears as a directory beneath it:

```bash
ls /omnifs
```

## Read a path

Try the DNS provider, which needs no credentials:

```bash
cat /omnifs/dns/example.com/MX
```

Use normal tools on the result:

```bash
cat /omnifs/dns/example.com/raw | jq .
```

If you configured GitHub, list open issues and read a field:

```bash
ls /omnifs/github/0xff-ai/omnifs/issues/open
cat /omnifs/github/0xff-ai/omnifs/issues/open/42/title
```

The first read may perform a host-mediated callout. Later reads can hit the view cache or render from cached canonical object bytes.

## Inspect what happened

Use `status`, `logs`, and `inspect` when a path surprises you:

```bash
omnifs status
omnifs logs -f
omnifs inspect --plain
```

`inspect` shows local runtime events: provider calls, callouts, cache behavior, and errors. It is the fastest way to see whether a read came from cache or reached an upstream system.

## Stop the runtime

```bash
exit
omnifs down
```

Next, read [the trust model](../security/the-trust-model.md) for the host/provider boundary or [Provider catalogue](/providers/) for provider path examples.
