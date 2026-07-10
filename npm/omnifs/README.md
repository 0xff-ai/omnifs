# omnifs

`omnifs` projects external services as native filesystems. The npm package
installs the native `omnifs` CLI and daemon binary for your host platform.

```bash
npm install -g @0xff-ai/omnifs
omnifs init github
omnifs up
omnifs shell
```

The npm install step does not start the daemon or fetch assets for an optional
virtualized FUSE frontend. The Rust CLI resolves those assets when the frontend
is requested. The daemon itself always runs on the host.

Supported npm host binaries:

- `darwin-arm64`
- `darwin-x64`
- `linux-arm64`
- `linux-x64`
