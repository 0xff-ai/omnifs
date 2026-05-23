# NPM distribution

The npm package is a host CLI installer. It must not become a second lifecycle
implementation. The Rust `omnifs` binary remains responsible for resolving
mount configs, materializing credentials, pulling the version-matched Docker
image, creating the privileged runtime container, and opening a shell inside it.

The package graph is intentionally small:

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
binary in the installed platform package. Platform packages contain only the
compiled Rust CLI and package metadata.

The npm install step does not pull `ghcr.io/raulk/omnifs:<version>`. That image
pull happens in `omnifs up`, where the CLI can honor `--image` and
`OMNIFS_IMAGE`, report Docker errors with runtime context, and avoid a large
download during package installation.

macOS and Linux use the same CLI-managed Docker flow:

```text
npm install -g @0xff-ai/omnifs
omnifs init github
omnifs up
omnifs shell
```

The platform difference is where the FUSE mount lives. On Linux the Docker
daemon uses the host Linux kernel. On macOS the daemon runs inside Docker
Desktop's Linux VM, so `/omnifs` is available inside the omnifs container rather
than as a native Finder or host-shell mount.

Release versions must stay lockstep:

```text
Cargo workspace version
npm root package version
npm platform package versions
ghcr.io/raulk/omnifs:<version>
```
