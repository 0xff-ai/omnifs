# omnifs-cli

The `omnifs` command-line tool: mount [omnifs](https://github.com/0xff-ai/omnifs) providers as a FUSE filesystem so external services (GitHub, arXiv, DNS, your own) appear as ordinary files and directories.

## Install

```bash
npm install -g @0xff-ai/omnifs
```

The npm package installs the native host CLI for Linux and macOS. The CLI then pulls the version-matched runtime image from `ghcr.io/0xff-ai/omnifs` when you run `omnifs up`.

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

The CLI stores credentials and thin mount configs on the host, starts the Docker runtime container, and opens a shell where `/omnifs` is mounted.

## Platform

The host CLI ships for Linux and macOS. The runtime filesystem is Linux FUSE inside Docker. On macOS, `omnifs shell` is the supported access path; omnifs does not install a native macOS Finder mount.

## Status

Pre-1.0. CLI surface and config format may evolve before v1.

## Configuration file

Optional. Lives at `~/.omnifs/config/config.toml` by default, or `$OMNIFS_HOME/config/config.toml` when `OMNIFS_HOME` is set.

Precedence: CLI flag > env var > config file > built-in default.

```toml
container_name = "omnifs"
image = "ghcr.io/0xff-ai/omnifs:0.4"

[paths]
mounts_dir = "~/work/omnifs-mounts"
providers_dir = "~/.omnifs/data/providers"
```

`~/` in path values expands against `$HOME`.

## License

Dual licensed under MIT or Apache-2.0 at your option.
