# omnifs-cli

The `omnifs` command-line tool and host-native daemon. It mounts [omnifs](https://github.com/0xff-ai/omnifs) providers so external services such as GitHub, arXiv, and DNS appear as ordinary files and directories.

## Install

```bash
npm install -g @0xff-ai/omnifs
```

The npm package installs the native `omnifs` binary for Linux and macOS. `omnifs up` starts the hidden host-native daemon and launches configured frontends (or platform defaults). Providers, credentials, and caching never run in a container.

Frontends are slim runners (`omnifs-fuse`, `omnifs-nfs`) delivered by local, Docker, or krunkit drivers. The Docker FUSE frontend uses the version-matched `ghcr.io/0xff-ai/omnifs-frontend:<version>` image. Local development uses `omnifs-frontend:dev` and never pulls it.

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

The CLI stores credentials and self-contained mount specs under `OMNIFS_HOME`. The daemon runs on the host. Frontends are declared in `[[frontends]]` (or defaults); `omnifs frontend up` manages a specific one. `omnifs shell` prefers Docker, then krunkit, then local.

## Platform

Linux defaults to local FUSE. macOS defaults to local NFS plus Docker FUSE. Defaults are replaced entirely by an explicit `[[frontends]]` list. Drivers are local (host), docker, and krunkit (macOS FUSE only). All attach to the host-native daemon over the wire protocol.

## Status

Pre-1.0. CLI surface and config format may evolve before v1.

## Configuration file

Optional. Lives at `~/.omnifs/config.toml` by default, or `$OMNIFS_HOME/config.toml` when `OMNIFS_HOME` is set. Run `omnifs setup` to create it; use command help for current overrides rather than copying version-specific runtime settings.

## License

Dual licensed under MIT or Apache-2.0 at your option.
