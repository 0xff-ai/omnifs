# Distribution

omnifs ships as four coupled artifacts at one version: the host CLI, the WASM providers, the npm packages, and the runtime image.

The CLI installs from npm. `@0xff-ai/omnifs` is the entry package, and it resolves a platform-specific binary from one of four optional dependencies (macOS arm64 and x64, Linux arm64 and x64). The npm install brings down the native CLI only. The Docker runtime image is pulled on `omnifs up`, not at install, so installing stays fast and Docker errors surface where you can act on them.

The runtime image lives at `ghcr.io/0xff-ai/omnifs`, tagged with the same unprefixed semver as the CLI. Version coupling is strict: for release X.Y.Z, the npm version, the CLI's `--version`, and the default image tag are all X.Y.Z. The CLI, daemon, and image move together, so do not mix versions.

Providers and tools are published as release assets and installed into `OMNIFS_HOME/providers`. The runtime image is not the owner of the provider directory. The host home is.
