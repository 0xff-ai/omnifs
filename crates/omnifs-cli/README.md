# omnifs-cli

The `omnifs` command-line tool and host-native daemon. It mounts [omnifs](https://github.com/0xff-ai/omnifs) providers so external services such as GitHub, arXiv, and DNS appear as ordinary files and directories.

## Install

```bash
npm install -g @0xff-ai/omnifs
```

The npm package installs the native `omnifs` binary for Linux and macOS. `omnifs up` starts the hidden host-native daemon; frontends are independent runners that you start with `omnifs frontend enable`. Providers, credentials, and caching never run in a container.

Frontends use the slim `omnifs-thin` runner in `fuse` or `nfs` mode, running in host, Docker, or krunkit environments. The Docker FUSE frontend uses the version-matched `ghcr.io/0xff-ai/omnifs-frontend:<version>` image. Local development uses `omnifs-frontend:dev` and never pulls it.

Binary releases for Linux and macOS are also attached to each [GitHub Release](https://github.com/0xff-ai/omnifs/releases).

From source, use:

```bash
cargo install omnifs-cli
```

## Quick start

```bash
omnifs mount add github
omnifs up
omnifs frontend enable fuse --environment docker
omnifs frontend shell fuse --environment docker
```

The CLI stores credentials and self-contained mount specs under `OMNIFS_HOME`. The daemon runs on the host. Frontends are independent access surfaces over the complete shared namespace; observe and manage them with `omnifs frontend ls`, `enable`, `disable`, and `restart`. Host frontends are ordinary mounted paths; `omnifs frontend shell` enters one explicit Docker or krunkit frontend.

## Platform

Enable a host, Docker, or krunkit frontend explicitly. Host locations must be absolute; Docker and krunkit own their guest location:

```bash
omnifs frontend enable nfs --environment host --location "/Users/me/omnifs"
omnifs frontend enable fuse --environment docker
omnifs frontend enable fuse --environment krunkit
omnifs frontend restart fuse --environment docker
omnifs frontend disable fuse --environment docker
```

Docker and krunkit deliver FUSE only. Every frontend attaches to the host-native daemon over the wire protocol and exposes every mount.

## Output

Global `--output human|json|jsonl` selects the output contract. JSON emits one envelope with plural resource arrays such as `result.frontends`, `result.mounts`, and `result.providers`; JSONL emits the same single terminal result or error with a stream-record discriminator. Finite structured commands never emit progress records. `--quiet`, `--no-input`, and `--yes` are also invocation-wide.

## Status

Pre-1.0. CLI surface and config format may evolve before v1.

## Configuration file

Optional. Lives at `~/.omnifs/config.toml` by default, or `$OMNIFS_HOME/config.toml` when `OMNIFS_HOME` is set. Run `omnifs setup` to create it; use command help for current overrides rather than copying version-specific runtime settings.

## License

Dual licensed under MIT or Apache-2.0 at your option.
