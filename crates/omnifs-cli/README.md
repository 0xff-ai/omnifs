# omnifs-cli

The `omnifs` command-line tool and host-native daemon. It mounts [omnifs](https://github.com/0xff-ai/omnifs) providers so external services such as GitHub, arXiv, and DNS appear as ordinary files and directories.

## Install

```bash
npm install -g @0xff-ai/omnifs
```

The npm package installs the native `omnifs` binary for Linux and macOS. `omnifs up` starts its hidden host-native daemon mode; providers, credentials, and caching never run in a container.

The optional Docker frontend uses the version-matched `ghcr.io/0xff-ai/omnifs-frontend:<version>` image. Local development uses `omnifs-frontend:dev` and never pulls it.

Binary releases for Linux and macOS are also attached to each [GitHub Release](https://github.com/0xff-ai/omnifs/releases).

From source, use:

```bash
cargo install omnifs-cli
```

## Quick start

```bash
omnifs init github
omnifs up
omnifs shell
```

The CLI stores credentials and self-contained mount specs under `OMNIFS_HOME`. The daemon runs on the host. `omnifs frontend up` optionally attaches a credential-free virtualized FUSE frontend, and `omnifs shell` enters it at `/omnifs`.

## Platform

Linux defaults to native FUSE. macOS defaults to read-only NFSv4.0 loopback. The optional virtualized FUSE frontend uses Docker by default; macOS also supports `--driver krunkit`. Both attach to the same host-native daemon namespace.

## Status

Pre-1.0. CLI surface and config format may evolve before v1.

## Configuration file

Optional. Lives at `~/.omnifs/config.toml` by default, or `$OMNIFS_HOME/config.toml` when `OMNIFS_HOME` is set. Run `omnifs setup` to create it; use command help for current overrides rather than copying version-specific runtime settings.

## License

Dual licensed under MIT or Apache-2.0 at your option.
