# omnifs-host

Host-side support for the [omnifs](https://github.com/0xff-ai/omnifs) virtual filesystem. It owns mount resolution, shared cache records, path-key indexing, and provider registry wiring used by the CLI and runtime adapters.

## What it does

- **Provider registry**: loads resolved mounts, instantiates provider runtimes, and manages timer-driven refresh tasks.
- **Mount resolution**: resolves raw mount specs into runtime-ready mounts through `omnifs_host::mounts`.
- **Cache protocol**: defines host cache records and conversion helpers shared by provider runtime and FUSE.
- **Path indexing**: maps mount-relative paths to inode state for FUSE invalidation.

## Platform

The runtime FUSE mount is Linux-only. The host CLI can run on macOS and Linux by talking to the Linux container runtime path.

## Install

```toml
[dependencies]
omnifs-host = "0.1"
```

## Status

Pre-1.0. Library API may evolve.

## License

Dual licensed under MIT or Apache-2.0 at your option.
