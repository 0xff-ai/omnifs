---
title: Managing mounts
description: Add, list, enable, disable, and remove provider mounts. Reset wipes configs and optionally credentials.
---

A mount binds a provider to a path under the omnifs root (for example the GitHub
provider at `/github`). `omnifs init` sets up mounts, `omnifs mounts` manages
them, and `omnifs reset` clears configuration.

```bash
omnifs init github     # add a mount for one provider
omnifs mounts list     # see what is configured
```

## Adding a mount

`omnifs init <provider>` initializes a provider's mount and credentials. Omit the
provider to set up all built-in providers. Use `-y` to accept defaults without
prompts.

```bash
omnifs init             # set up all built-in providers
omnifs init github      # just GitHub
omnifs init github -y   # non-interactive
```

You can also add a mount directly with `omnifs mounts add`, optionally choosing
the mount path or supplying a JSON config file.

```bash
omnifs mounts add github
omnifs mounts add github --path /github
omnifs mounts add db --config ./my-db-mount.json
```

## Listing and toggling mounts

`omnifs mounts list` shows configured mounts. Add `--all` to include disabled
ones. Enable or disable a mount without deleting it.

```bash
omnifs mounts list
omnifs mounts list --all
omnifs mounts disable docker
omnifs mounts enable  docker
```

## Removing a mount

Remove a single mount by its path or provider.

```bash
omnifs mounts remove linear
omnifs mounts remove /linear
```

## Where mount configs live

Mount configs are JSON files under `~/.omnifs/config/mounts`. The config tree
sits in `~/.omnifs/config`, and runtime data (including the credentials file
fallback) lives in `~/.omnifs/data`.

| Location | Contents |
|----------|----------|
| `~/.omnifs/config/mounts` | One JSON config file per mount |
| `~/.omnifs/config` | Configuration root |
| `~/.omnifs/data` | Runtime data (e.g. `credentials.json` fallback) |

:::note
Set `OMNIFS_HOME` to relocate the entire `~/.omnifs` tree.
:::

## Resetting everything

`omnifs reset` removes omnifs configuration. By default it leaves credentials in
place; pass `--credentials` to remove those too. Use `-y` to skip the
confirmation prompt.

```bash
omnifs reset                 # remove configuration, keep credentials
omnifs reset --credentials   # remove configuration and credentials
omnifs reset --credentials -y
```

:::danger
`omnifs reset --credentials` deletes stored credentials and mount configs. You
will need to re-run `omnifs init` and re-authenticate afterward. There is no
undo.
:::
