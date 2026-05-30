---
title: Quickstart
description: Configure GitHub and Linear, start the omnifs container, and explore the world as files in a few commands.
---

This is the happy path from a fresh install to browsing services as files. If
you have not installed the CLI yet, start with
[Install](/getting-started/install/).

## 1. Configure providers

`omnifs init <provider>` runs an interactive setup for a single provider: it
runs OAuth if the provider needs it, validates the credential, and writes a thin
mount config under `~/.omnifs/config/mounts`.

```bash
omnifs init github
omnifs init linear
```

- **GitHub** uses device-code OAuth with the bundled public client id and no
  default write scopes. You will be shown a code to enter in your browser.
- **Linear** uses browser PKCE OAuth with the bundled public client id and the
  `read` scope.

Both providers are read-only by default. You do not need to edit any config,
copy provider wasm into place, or set OAuth client id environment variables for
the bundled providers.

:::tip
Run `omnifs init` with no argument to pick a provider from an interactive list.
Pass `--no-auth` to write the mount config without running the credential step.
:::

## 2. Check status

```bash
omnifs status
```

This prints your configured mounts, the providers behind them, and current auth
state — a fast way to confirm everything is wired up before starting the
container.

## 3. Start the container

```bash
omnifs up
```

The first run pulls the version-matched runtime image and starts the container.
Override the image with `--image` or `OMNIFS_IMAGE` if needed.

## 4. Open a shell and explore

```bash
omnifs shell
```

You are now inside the container, where the omnifs filesystem is mounted. The
mount lives inside the container — on macOS it is not a native Finder mount, so
`omnifs shell` is how you reach it.

### GitHub as files

```bash
# List repos in a user or org
> cd /github/torvalds
> ls
1590A       GuitarPedal       libdc-for-dirk  linux       subsurface-for-dirk  uemacs
AudioNoise  HunspellColorize  libgit2         pesconvert  test-tlb

# cd into a repo
> cd /github/ollama/ollama
> ls
actions  issues  pulls  repo

# clone the repo just by listing it (cloned on demand over SSH)
> cd /github/ollama/ollama/repo
> ls
CMakeLists.txt     Makefile.sync  cmd        go.mod       llm         openai    server
CMakePresets.json  README.md      convert    go.sum       logutil     parser    template

# list open issues
> cd /github/ollama/ollama/issues/open
> ls
10333  10928  11381  11743  12138  12539  12959  13399  13879  14239  14621
```

:::note
Repo trees under `/github/<owner>/<repo>/repo/` are cloned on demand over SSH.
That path needs an SSH agent with a GitHub key loaded — see
[Prerequisites](/getting-started/prerequisites/).
:::

## 5. Inspect and stop

```bash
omnifs logs        # show container output
omnifs logs -f     # follow it
omnifs down        # stop and remove the container
```

## Next steps

- Dig into provider path layouts in [Providers](/providers/).
- Learn day-to-day workflows in [Guides](/guides/).
- Prefer a guided, re-runnable setup? See
  [Guided onboarding](/getting-started/guided-onboarding/).
