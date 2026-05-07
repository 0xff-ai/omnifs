# omnifs-cli

The `omnifs` command-line tool: mount [omnifs](https://github.com/raulk/omnifs) providers as a FUSE filesystem so external services (GitHub, arXiv, DNS, your own) appear as ordinary files and directories.

## Install

```bash
cargo install omnifs-cli
```

Binary releases for `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu` are also attached to each [GitHub Release](https://github.com/raulk/omnifs/releases). A Docker image is available at `ghcr.io/raulk/omnifs`.

Requires `fuse3` and `libfuse3-dev` on the host (`apt install fuse3 libfuse3-dev` on Debian / Ubuntu).

## Quick start

```bash
omnifs mount ~/.omnifs/config.json /omnifs
ls /omnifs/github/raulk/omnifs/issues/
cat /omnifs/dns/google.com/A
```

Mount config is JSON, one entry per provider, with the provider's wasm path, mount prefix, and provider-specific config.

## Platform

Linux only today. macOS (macFUSE) and Windows (WinFsp) targets are tracked as follow-up work.

## Status

Pre-1.0. CLI surface and config format may evolve before v1.

## License

Dual licensed under MIT or Apache-2.0 at your option.
