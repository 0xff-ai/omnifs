# omnifs-cli

The `omnifs` command-line tool and host-native daemon. It mounts [omnifs](https://github.com/0xff-ai/omnifs) providers so external services such as GitHub, arXiv, and DNS appear as ordinary files and directories.

## Install

```bash
npm install -g @0xff-ai/omnifs
```

The npm package installs the native `omnifs` binary for Linux and macOS. `omnifs up` starts the hidden host-native daemon; named filesystems are independent runners managed with `omnifs fs`. Providers, credentials, and caching never run in a container.

Host filesystems run through the full binary's hidden `omnifs run-fs` command. Docker and libkrun guests use the slim `omnifs-thin` runner. The Docker FUSE filesystem uses the version-matched `ghcr.io/0xff-ai/omnifs-filesystem:<version>` image. Local development uses `omnifs-filesystem:dev` and never pulls it.

Binary releases for Linux and macOS are also attached to each [GitHub Release](https://github.com/0xff-ai/omnifs/releases).

From source, use:

```bash
cargo install omnifs-cli
```

## Quick start

```bash
omnifs setup --providers github
omnifs fs ls
```

`omnifs setup` is a thin first-run composition: it configures exact embedded providers, starts the daemon, creates named platform filesystems, and attaches them. `--no-up` configures mounts without launching lifecycle actions. The CLI stores credentials, mount specs, and filesystem specs under `OMNIFS_HOME`.

## Platform

Create and attach a host, Docker, or libkrun filesystem explicitly. Host locations must be absolute; Docker and libkrun own their guest location:

```bash
omnifs fs create --name local --protocol nfs --runtime host --location "/Users/me/omnifs"
omnifs fs create --name docker --runtime docker
omnifs fs create --name vm --runtime libkrun
omnifs fs attach --name local
omnifs fs attach --name docker
omnifs fs restart --name docker
omnifs fs detach --name docker
```

Docker and libkrun deliver FUSE only. Every filesystem attaches to the host-native daemon over the wire protocol and exposes every mount.

## Output

Global `--output human|json|jsonl` selects the output contract. JSON emits one envelope with plural resource arrays such as `result.filesystems`, `result.mounts`, and `result.providers`; JSONL emits the same single terminal result or error with a stream-record discriminator. Finite structured commands never emit progress records. `--quiet`, `--no-input`, and `--yes` are also invocation-wide.

## Status

Pre-1.0. CLI surface and config format may evolve before v1.

## Configuration file

Optional. Lives at `~/.omnifs/config.toml` by default, or `$OMNIFS_HOME/config.toml` when `OMNIFS_HOME` is set. The CLI uses defaults when it is absent; use command help for current overrides rather than copying version-specific runtime settings.

## License

Dual licensed under MIT or Apache-2.0 at your option.
