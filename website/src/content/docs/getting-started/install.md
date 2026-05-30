---
title: Install
description: Install the omnifs CLI from npm. The package installs only the native host binary; the runtime image is pulled on omnifs up.
---

Install the omnifs CLI globally with npm:

```bash
npm install -g @0xff-ai/omnifs
```

Confirm it is on your `PATH`:

```bash
omnifs --version
```

## What npm installs

The npm package is a host CLI installer and nothing more. It installs only the
native `omnifs` binary for your platform — it does **not** pull the Docker
runtime image at install time. That keeps the install small and avoids a large
download before you have configured anything.

The package graph is intentionally minimal: the root `@0xff-ai/omnifs` package
exposes the `omnifs` executable and delegates to the matching platform package
(`@0xff-ai/omnifs-cli-darwin-arm64`, `-darwin-x64`, `-linux-arm64`, or
`-linux-x64`), which carries the compiled binary for your OS and CPU.

## When the runtime image is pulled

The runtime image is pulled the first time you run `omnifs up`, not during
`npm install`. By deferring the pull, the CLI can report Docker errors with
runtime context and honor an image override.

By default `omnifs up` uses the version-matched runtime image — the image tag
matches your installed CLI version. Override it when you need a specific image:

```bash
omnifs up --image ghcr.io/0xff-ai/omnifs:0.2.0
# or
OMNIFS_IMAGE=ghcr.io/0xff-ai/omnifs:0.2.0 omnifs up
```

Only `omnifs up` touches Docker. `npm install` and `omnifs init` run entirely
on the host.

## Next steps

- Run through the [Quickstart](/getting-started/quickstart/) for the full happy
  path.
- Prefer a guided walkthrough? Use
  [Guided onboarding](/getting-started/guided-onboarding/) (`omnifs setup`).
