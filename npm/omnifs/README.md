# omnifs

`omnifs` is a filesystem projection system. It projects external services into
a shared virtual namespace that can be exposed as one or more filesystems. The
npm package installs the native `omnifs` CLI and daemon binary for your host
platform.

```bash
npm install -g @0xff-ai/omnifs
omnifs setup --providers github
omnifs status
```

The npm install step does not start the daemon or fetch assets for an optional
virtualized FUSE filesystem. The Rust CLI resolves those assets when the filesystem
is requested. The daemon itself always runs on the host.

Supported npm host binaries:

- `darwin-arm64`
- `darwin-x64`
- `linux-arm64`
- `linux-x64`
