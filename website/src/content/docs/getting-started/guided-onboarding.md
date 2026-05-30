---
title: Guided onboarding
description: omnifs setup walks you through OS detection, Docker, provider selection, per-provider init, and launching the container.
---

`omnifs setup` is a single guided walkthrough that takes you from a fresh
install to a running container. It is the easiest way to onboard if you would
rather be led step by step than run individual commands.

```bash
omnifs setup
```

## What it does

`omnifs setup` runs these steps in order:

1. **Detect your OS.** It identifies whether you are on Linux, macOS, or another
   platform so it can tailor the rest of the walkthrough.
2. **Explain Docker.** It explains that omnifs runs its filesystem inside a Linux
   container, with copy specific to your OS:
   - On Linux, Docker uses the host kernel directly and the mount lives inside
     the container.
   - On macOS, Docker Desktop runs a Linux VM; the mount lives inside that VM's
     container and is reached through `omnifs shell`, not as a native Finder
     mount.
3. **Pick providers.** It shows a multi-select picker of available providers.
   Providers you have already configured are listed but excluded from the
   picker, so you only choose new ones.
4. **Init each selected provider.** For every provider you pick, it runs the same
   interactive init used by `omnifs init` — running OAuth if required, validating
   the credential, and writing the mount config.
5. **Launch the container.** It finishes by running `omnifs up` to start the
   runtime container.

If you select no providers, setup skips the init steps and still brings the
container up; you can run `omnifs init <provider>` later.

## Re-runnable

`omnifs setup` is safe to run again. Because already-configured providers are
excluded from the picker, re-running it lets you add providers incrementally
without re-doing the ones you have already set up.

## Next steps

After setup finishes the container is running. Open a shell and explore:

```bash
omnifs shell
```

See the [Quickstart](/getting-started/quickstart/) for example paths to browse,
and [Providers](/providers/) for the full path layouts.
