# NPM distribution

The npm package is a host CLI installer. It must not become a second lifecycle
implementation. The Rust `omnifs` binary remains responsible for resolving
mount configs, materializing credentials, selecting the runtime mode, launching
the filesystem frontend, and opening a shell at the mount root.

The package graph stays small:

```text
@0xff-ai/omnifs
  bin/omnifs.js
  optionalDependencies:
    @0xff-ai/omnifs-cli-darwin-arm64
    @0xff-ai/omnifs-cli-darwin-x64
    @0xff-ai/omnifs-cli-linux-arm64
    @0xff-ai/omnifs-cli-linux-x64
```

The root package exposes the `omnifs` executable and delegates to the native
binary in the installed platform package. Platform packages currently contain
the compiled Rust CLI and package metadata.

Runtime mode is a CLI concern:

```text
npm install -g @0xff-ai/omnifs
omnifs init github
omnifs setup --mode native # persists native; use --mode docker for Docker
omnifs up                  # loads the persisted runtime mode
```

Release commands default to `auto` until setup persists either `native` or
`docker`; contributor `omnifs dev` defaults to Docker for CI parity only when no
mode has been persisted. Docker mode pulls `ghcr.io/0xff-ai/omnifs:<version>`
during `omnifs up`, not during `npm install`. Keeping the pull in the CLI lets
it honor `--image` and `OMNIFS_IMAGE`, report Docker errors with runtime
context, and avoid a large download during package installation.

Native mode must not depend on a Docker image. A packaged native release needs
provider WASM sidecars available to the host CLI, either inside the platform
packages or in a provider package resolved by the root package. The CLI can then
run the same provider host locally, using NFSv4 loopback on macOS and FUSE on
Linux. Until those sidecars ship through npm, native mode is explicit and
requires provider `.wasm` files in the configured providers directory.

Release versions must stay lockstep:

```text
Cargo workspace version
npm root package version
npm platform package versions
provider WASM sidecar versions
ghcr.io/0xff-ai/omnifs:<version>
```
