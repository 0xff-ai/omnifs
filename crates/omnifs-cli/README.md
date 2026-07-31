# omnifs-cli

The `omnifs` command and host-native daemon. It projects external services such
as GitHub, arXiv, and DNS as ordinary files and directories.

## Install

```bash
npm install -g @0xff-ai/omnifs
```

The npm package installs the native `omnifs` binary for Linux and macOS.
Binary releases are also attached to each
[GitHub Release](https://github.com/0xff-ai/omnifs/releases). From source:

```bash
cargo install omnifs-cli
```

## Quick start

```bash
omnifs setup
omnifs status
omnifs attachment ls
```

`omnifs setup` starts the daemon, lists embedded providers, and offers a small
initial desired resource set. The daemon stores resources, provider artifacts,
credential material, action receipts, caches, logs, and observed Attachment
state under `OMNIFS_HOME/daemon-state`.

## Resources and automation

Interactive `provider`, `mount`, `credential`, and `attachment` commands edit
the complete desired set, show the typed plan, ask for consent, apply one
SQLite transaction, and follow daemon progress to a terminal revision or
action.

KCL is the automation surface:

```bash
omnifs config init > omnifs.k
omnifs plan omnifs.k
omnifs apply omnifs.k --yes
```

The client evaluates KCL in process and converts its result to strict Rust
resource types. KCL never contains secrets. Static-token automation uses only
`omnifs credential set NAME --from-env VARIABLE`.

## Attachments

An Attachment is one desired OS-facing exposure of the complete shared
namespace. Resource presence asks the daemon to keep its filesystem runtime
attached. Removing the resource asks the daemon to stop the exact runtime and
VFS session.

```bash
omnifs attachment add
omnifs attachment ls
omnifs attachment show local
omnifs attachment restart local
omnifs attachment shell local -- ls -la /omnifs
omnifs attachment rm local
```

Host Attachments use FUSE on Linux or NFSv4 loopback on macOS. Docker and
libkrun Attachments use FUSE at `/omnifs`. Every Attachment exposes every
configured Mount.

## Progress and output

Human and JSONL mutations wait and stream typed progress by default. JSON waits
and emits one terminal envelope. Quiet human mode waits and prints only the
terminal receipt. Non-TTY human progress uses stable lines without cursor
control. Ctrl-C stops only the viewer, exits 130, and prints the exact revision
or action follow command while daemon work continues.

Global `--output human|json|jsonl`, `--quiet`, `--no-input`, and `--yes` apply
to the full invocation. Read commands return plural resource arrays and
absolute machine paths.

## Configuration

Optional profile config lives at `~/.omnifs/config.toml`, or
`$OMNIFS_HOME/config.toml` when `OMNIFS_HOME` is set. Use command help for the
current surface.

## Status

Pre-1.0. CLI and config formats may change before v1.

## License

Dual licensed under MIT or Apache-2.0 at your option.
