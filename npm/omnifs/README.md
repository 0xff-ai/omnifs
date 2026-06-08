# omnifs

`omnifs` is the host CLI for omnifs. The npm package installs a native `omnifs`
binary for your host platform. The CLI owns runtime startup in both Docker mode
and native mode.

```bash
npm install -g @0xff-ai/omnifs
omnifs init github
omnifs up
omnifs shell
```

The npm install step does not pull the Docker image. Docker image selection and
pull remain owned by the Rust CLI so Docker mode can use the version-matched
runtime image, honor `--image` and `OMNIFS_IMAGE`, materialize credentials, and
report Docker errors in one place.

Docker mode remains the default packaged runtime until setup persists a runtime
choice. `omnifs setup --mode native` or `omnifs setup --mode docker` writes the
selected mode to `~/.omnifs/config/config.toml`; later runtime commands load it
automatically. Native mode runs the provider host on this machine and mounts
`~/OmniFS` by default:

```bash
omnifs setup --mode native
omnifs up
omnifs shell
```

You can inspect or change the same default directly:

```bash
omnifs config runtime --mode native
omnifs config runtime --mode docker
```

Native mode currently requires provider `.wasm` components in the configured
providers directory. Docker mode remains available on macOS and Linux.

Supported npm host binaries:

- `darwin-arm64`
- `darwin-x64`
- `linux-arm64`
- `linux-x64`
