# Native CI pipeline plan

## Overall judgment

This is the chosen destination for the release pipeline. The current Dockerfile does two jobs with different invariants: Rust build system and runtime image producer. Docker should package and smoke the Linux runtime environment. Cargo should build Rust artifacts.

The invariant is:

```text
source commit -> native Cargo artifacts -> attested files -> runtime image assembly -> smoke -> release promotion
```

This replaces Docker-as-builder with an artifact-first model. The CLI binary, provider WASM components, checksums, and provenance become explicit CI outputs. The Docker image becomes a consumer of those outputs.

The gating risk is Linux binary compatibility. Native CI must prove the Linux CLI ABI and FUSE linkage model before any Docker Rust build stages are deleted. `cargo-zigbuild` is the tool that can make this practical: it gives Linux builds an explicit glibc baseline and can also cross-link macOS artifacts from Linux. If Phase 0 fails, keep the contributor Docker path intact and design a direct fallback instead of reviving the old Docker build graph.

## Phase 0 gates

Do not commit to the native pipeline until these gates are green on both `linux-x64` and `linux-arm64`:

1. Build Linux CLI binaries with `cargo-zigbuild` and a fixed glibc baseline:

   ```bash
   cargo zigbuild --release -p omnifs-cli --target x86_64-unknown-linux-gnu.2.17
   cargo zigbuild --release -p omnifs-cli --target aarch64-unknown-linux-gnu.2.17
   ```

   Local and CI spike runs should use `scripts/ci/build-linux-zigbuild.sh <target>` so the same pinned Zig and `cargo-zigbuild` versions, target setup, mold neutralization, and Linux ABI inspection are exercised every time.

2. Build macOS CLI binaries with `cargo-zigbuild` from Linux:

   ```bash
   cargo zigbuild --release -p omnifs-cli --target x86_64-apple-darwin
   cargo zigbuild --release -p omnifs-cli --target aarch64-apple-darwin
   ```

3. Pin the Phase 0-proven toolchain inputs:

   ```text
   cargo-zigbuild version
   Zig version
   cargo-zigbuild container digest if using ghcr.io/rust-cross/cargo-zigbuild
   macOS SDK source or container digest if cross-linking Darwin targets
   ```

   Record these pins in `tools/versions.toml`, and have CI setup scripts read that file. Do not use floating `latest` tags or unversioned `cargo install cargo-zigbuild`.

4. Run the produced Linux binary on the native GitHub runner and inside the runtime image for the same architecture:

   ```bash
   ./omnifs --version
   docker run --rm <runtime-digest> omnifs --version
   ```

5. Inspect ABI requirements and fail if the Linux CLI requires a newer glibc symbol than `GLIBC_2.17`:

   ```bash
   readelf -V ./omnifs
   ldd ./omnifs
   ```

6. Inspect dynamic FUSE linkage:

   ```bash
   ldd ./omnifs | rg 'fuse|libfuse'
   ```

7. On macOS, test the packaging surface produced by Linux cross-linking:

   ```text
   npm install from packed root package and run omnifs --version
   Homebrew install test once a tap/formula exists
   direct-download quarantine test only if direct GitHub Release downloads are documented as supported
   ```

8. Assemble temporary `linux/amd64` and `linux/arm64` runtime images from the native CLI binary plus native WASM artifacts.

   Local and CI spike runs should use `scripts/ci/build-runtime-image.sh` after the matching Linux binary and provider WASM artifacts have been built. This script assembles the runtime image from native artifacts only, so it exercises the new Docker-as-packager boundary without re-entering Docker Rust build stages.

9. Smoke the assembled runtime image by digest. Run amd64 on the standard Ubuntu runner and arm64 on `ubuntu-24.04-arm` if available.

10. Decide the FUSE packaging model before Phase 2:

   ```text
   chosen final shape: npm CLI is a separate thin crate without omnifs-host or fuser
   runtime image binary is a separate daemon crate that depends on omnifs-host and fuser
   fallback only: one Linux binary remains temporarily if ldd proves no libfuse dynamic link and size/startup are acceptable
   ```

These are not later hardening tasks. They decide whether native CI is viable for release packaging.

## Phase 0 deliverables checklist

Phase 0 is complete only when the PR records these artifacts:

1. Chosen `cargo-zigbuild`, Zig, SDK, and container digest pins in `tools/versions.toml`.
2. Linux `readelf -V` output proving no symbol newer than `GLIBC_2.17`.
3. Linux `ldd` output proving the actual FUSE linkage story.
4. A decision record for the separate daemon crate split, including whether any temporary single-binary fallback remains.
5. A `fuser` feature verdict: current `fuser 0.17` has `default = []`, `libfuse3 = ["libfuse"]`, and the current Omnifs dependency enables `macos-no-mount`; verify the Linux build uses the pure-Rust mount path unless the daemon deliberately enables `libfuse3`.
6. Nextest archive evidence: whether host tests runtime-load WASM from `target/wasm32-wasip2/release`, and the sidecar or `archive.include` mechanism used to make those files available in `host-test-run`.
7. macOS distribution decision: npm/Homebrew-only and unsigned, or direct downloads with signing/notarization.
8. Apple SDK decision: accept the rust-cross SDK path for this OSS distribution, or retain macOS builders.
9. Fixture cache evidence for `Chinook_Sqlite.sqlite` and the `ollama/ollama` smoke target, including checksums and hit/miss behavior.
10. Cold and warm timing baseline for the old pipeline and the Phase 0 native spike.
11. Runtime smoke output for amd64 and arm64 digests.
12. A note on whether smoke should expand beyond `OMNIFS_DEMO_MODE=smoke` to exercise any runtime path that could expose a native-link or `-sys` dependency issue.
13. The exact Phase 1 PR scope and rollback path.

## Target architecture

```text
Pull request
  preflight-fast
    cargo fmt --all --check
    actionlint
    zizmor
  preflight-policy
    release-check
    npm validate
  wasm build
    install WASI SDK 33 with checksum verification
    omnifs-tool-*.wasm
    omnifs-provider-*.wasm
    test-provider.wasm
    wasm-tools validate
  host verification
    download wasm artifacts
    clippy host
    clippy wasm
    nextest archive
    wasm sidecar for runtime-loaded test providers
    nextest run
    wasm test compile
  verify aggregate

Trusted main or ci-full run
  cli native builds on Linux with cargo-zigbuild
    x86_64-unknown-linux-gnu.2.17 through cargo-zigbuild
    aarch64-unknown-linux-gnu.2.17 through cargo-zigbuild
    x86_64-apple-darwin through cargo-zigbuild
    aarch64-apple-darwin through cargo-zigbuild
  optional macOS signing job
    codesign/notarytool only if direct macOS downloads require it
  artifact evidence
    checksums
    attestations
  runtime image assembly
    linux/amd64 image copies linux-x64 CLI + provider WASM
    linux/arm64 image copies linux-arm64 CLI + provider WASM
  amd64 smoke by digest
  arm64 smoke by digest when runner capacity is available
  multi-platform sha-<commit> manifest
  image scan
  image signature

Release workflow
  plan
  download artifacts from the triggering CI run
  verify checksums and attestations
  publish GitHub Release
  promote sha-<commit> image to X.Y.Z and vX.Y.Z
  publish npm through OIDC
  verify public npm, GitHub Release, and GHCR surfaces
```

The core change is that these release jobs stop invoking `cargo` inside Docker:

```text
lint
test
wasm-artifacts
cli-amd64
cli-arm64
runtime-amd64
runtime-arm64
```

Instead, native Cargo jobs produce a small set of files:

```text
dist/wasm/omnifs_provider_*.wasm
dist/wasm/omnifs_tool_*.wasm
dist/cli/linux-x64/omnifs
dist/cli/linux-arm64/omnifs
dist/cli/darwin-x64/omnifs
dist/cli/darwin-arm64/omnifs
dist/checksums/omnifs-SHA256SUMS
```

The Docker build consumes only the Linux daemon binary, WASM providers, shell scripts, and runtime package list.

## What becomes simpler

| Current shape | Native shape |
|---|---|
| Docker builds Rust dependencies, host CLI, provider WASM, and runtime image. | Cargo builds Rust artifacts. Docker assembles the runtime image. |
| BuildKit cache refs try to act like Rust dependency caches. | `Swatinem/rust-cache` caches Cargo output directly. |
| Linux CLI tarballs are side effects of Docker build targets. | CLI tarballs are first-class release artifacts from native build jobs. |
| Runtime image provenance proves a Docker build happened. | Runtime image provenance can be tied back to exact attested CLI and WASM inputs. |
| Release has to know which Docker lane produced which binary. | Release downloads named platform artifacts from the triggering CI run. |

The tradeoff is explicit ownership of Linux compatibility. Docker currently hides an implicit glibc baseline by building inside Debian. Native CI must set, test, and document that baseline.

## Linux ABI baseline

The Linux npm CLI and the Linux runtime daemon must be built against glibc 2.17 through `cargo-zigbuild`:

```text
x86_64-unknown-linux-gnu.2.17
aarch64-unknown-linux-gnu.2.17
```

This is the release baseline unless Phase 0 proves a dependency cannot be linked correctly. It avoids the `ubuntu-latest` trap where a binary built on Ubuntu 24.04 can require `GLIBC_2.39` and fail on Ubuntu 22.04, Debian 12, RHEL 9, and older enterprise distributions.

Do not substitute `ubuntu-latest` Cargo builds for Linux release artifacts. The GitHub runner is allowed to host the build; `cargo-zigbuild` owns the link baseline.

The CI check should fail on any symbol newer than the baseline:

```bash
readelf -V dist/cli/linux-x64/omnifs | rg 'GLIBC_'
readelf -V dist/cli/linux-arm64/omnifs | rg 'GLIBC_'
```

## Zigbuild role

`cargo-zigbuild` is a Cargo wrapper, not a replacement build system. Rust still compiles crates normally. The link step goes through `zig cc`, which can resolve Linux GNU symbols against a chosen glibc baseline and can link Darwin targets from Linux when the macOS SDK is available.

Use it for three release jobs:

```text
linux-x64:
  cargo zigbuild --release -p omnifs-cli --target x86_64-unknown-linux-gnu.2.17

linux-arm64:
  cargo zigbuild --release -p omnifs-cli --target aarch64-unknown-linux-gnu.2.17

darwin:
  cargo zigbuild --release -p omnifs-cli --target x86_64-apple-darwin
  cargo zigbuild --release -p omnifs-cli --target aarch64-apple-darwin
```

The preferred implementation is one Linux artifact job that builds all four CLI targets, then uploads four platform archives. If that job becomes too slow, split Linux and Darwin into two Linux jobs, but keep macOS builders out of the compile path.

Pin the exact execution environment. Valid shapes:

```text
direct runner install:
  setup-zig at a pinned version
  cargo-zigbuild at a pinned version with --locked
  explicit macOS SDK install if building Darwin targets

containerized build:
  ghcr.io/rust-cross/cargo-zigbuild pinned by version and digest
  repository target/cache directories mounted for rust-cache reuse
```

The direct runner install is better for `Swatinem/rust-cache` integration. The container is simpler for Darwin cross-linking because the rust-cross image carries the SDK setup. Phase 0 should test both only if the first chosen path fails; otherwise pick one and pin it.

Zigbuild does not solve:

```text
libfuse3:
  still dynamic or static native-library linkage if deliberately enabled; solve by thin CLI or prove fuser pure-Rust mount path with ldd

macOS signing/notarization:
  still requires Apple tooling on macOS if direct downloads need notarization

non-libc native libraries:
  pkg-config and system-library dependencies still need headers and libs in the build environment
```

## macOS distribution policy

The chosen distribution policy for this plan is npm/Homebrew first. Linux-cross-built Darwin binaries may remain unsigned as long as the supported install surfaces are package-manager installs:

```text
npm:
  npm extracts the platform package; no browser quarantine attribute is expected

Homebrew:
  brew extracts the archive; unsigned CLI binaries are acceptable for CLI tools

direct GitHub Release download:
  not a supported user-facing install path until a macOS signing/notarization job exists
```

Release may still attach Darwin archives to GitHub Releases for package-manager consumption and provenance. Do not document "download and double-click" or direct browser-download UX unless CI also signs and notarizes the artifacts.

Phase 0 macOS tests are:

```bash
npm pack ./npm/platform/darwin-arm64
npm pack ./npm/omnifs
npm install ./npm/omnifs/*.tgz
omnifs --version
```

Repeat for `darwin-x64` on an Intel runner or local Intel Mac if available. If direct download support is later added, test a browser or `curl` download that receives `com.apple.quarantine`, then verify Gatekeeper behavior. That path requires notarization.

For Apple SDK policy, this plan accepts the rust-cross macOS SDK approach for this OSS distribution. If project ownership or legal review rejects that, retain macOS builders for Darwin compile jobs and keep the rest of the native Linux plan intact.

## Linux binary and FUSE model

The current repository shape is not yet a proven thin client:

```text
omnifs-cli -> omnifs-host -> fuser
```

`crates/cli/Cargo.toml` depends on `omnifs-host` unconditionally, and `crates/host/Cargo.toml` depends on `fuser`. Hidden daemon commands use `omnifs_host::mount` and run inside the runtime container. User-facing commands such as `up`, `shell`, `logs`, and `status` primarily talk to Docker, but the binary graph can still pull in local FUSE linkage.

Phase 0 must answer this with `ldd`, not assumption.

Chosen final shape:

```text
npm CLI binary:
  new or narrowed omnifs-cli crate
  Docker/session/auth/config/status commands
  no omnifs-host dependency
  no fuser dependency
  no local FUSE install required

runtime daemon binary:
  separate omnifs-daemon or omnifs-host-runtime crate
  daemon mount/unmount commands
  omnifs-host dependency
  fuser dependency
  copied into the Ubuntu runtime image where fuse3 is installed
```

Do not make the final split a `daemon` feature on `omnifs-cli`. Cargo features are unified, so a transitive dependency that enables daemon code can pull `omnifs-host` and `fuser` back into the user CLI build. Use a separate crate for the daemon-side binary and keep the npm CLI crate free of host-runtime dependencies.

The release artifacts should express the real deployment model:

```text
omnifs                  # user CLI, npm package payload
omnifs-daemon           # runtime image payload, may be hidden from users
```

There is one important escape valve. `fuser 0.17` has empty default features; `libfuse3` is behind `libfuse3 = ["libfuse"]`; and the current Omnifs host dependency enables `macos-no-mount`, not `libfuse3`. On Linux, `fuser` can use its pure-Rust mount path and shell out to `fusermount3` without dynamically linking `libfuse3`. Phase 0 should prove this with `cargo tree -e features` and `ldd`.

Temporary fallback shape:

```text
single omnifs binary remains for one release cycle
ldd proves no libfuse/libfuse3 dynamic dependency
size and startup remain acceptable for npm users
```

The fallback is acceptable only if the separate daemon crate is too risky for the first native pipeline PR. It should not become the final architecture, because an npm-installed client should not carry the daemon runtime graph when actual mounting happens in the container.

## Artifact boundary

The build boundary should be file based:

```text
wasm artifact:
  omnifs_provider_*.wasm
  omnifs_tool_*.wasm
  test_provider.wasm if host tests need it

cli artifact per platform:
  omnifs-cli-linux-x64.tar.xz
  omnifs-cli-linux-arm64.tar.xz
  omnifs-cli-darwin-x64.tar.xz
  omnifs-cli-darwin-arm64.tar.xz

daemon artifact per Linux platform if the split lands:
  omnifs-daemon-linux-x64.tar.xz
  omnifs-daemon-linux-arm64.tar.xz

evidence artifact:
  omnifs-SHA256SUMS
  attestations stored through GitHub artifact attestations
```

The runtime image should be built from an assembly context, not from the repository root:

```text
runtime-context/
  bin/omnifs
  bin/omnifs-daemon if split from the CLI
  providers/omnifs_provider_github.wasm
  providers/omnifs_provider_dns.wasm
  scripts/demo.sh
  scripts/container-entrypoint.sh
```

That context is tiny and stable. It removes source files, Cargo manifests, `target/`, and unrelated repo contents from the image assembly boundary.

## Dockerfile shape

Create a runtime-only Dockerfile:

```dockerfile
# syntax=docker.io/docker/dockerfile:1
FROM ubuntu:25.10 AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bash ca-certificates curl fuse3 gnupg jq \
        zsh git openssh-client procps \
        bat git-delta ripgrep util-linux \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /etc/apt/keyrings \
    && curl -fsSL https://repo.charm.sh/apt/gpg.key \
        | gpg --dearmor -o /etc/apt/keyrings/charm.gpg \
    && echo "deb [signed-by=/etc/apt/keyrings/charm.gpg] https://repo.charm.sh/apt/ * *" \
        > /etc/apt/sources.list.d/charm.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends gum \
    && rm -rf /var/lib/apt/lists/*

COPY bin/omnifs /usr/local/bin/omnifs
COPY providers/omnifs_provider_*.wasm /root/.omnifs/providers/
COPY scripts/demo.sh /tmp/demo.sh
COPY scripts/container-entrypoint.sh /usr/local/bin/omnifs-container-entrypoint
```

If the binary split lands, copy both the user CLI and daemon into the image:

```dockerfile
COPY bin/omnifs /usr/local/bin/omnifs
COPY bin/omnifs-daemon /usr/local/bin/omnifs-daemon
```

The entrypoint currently ends with `exec omnifs daemon mount ...`. Change that line to call the daemon binary:

```bash
exec "${OMNIFS_DAEMON_BIN:-omnifs-daemon}" daemon mount \
  --mount-point "$OMNIFS_MOUNT_POINT" \
  --config-dir "$OMNIFS_CONFIG_DIR" \
  --cache-dir "$OMNIFS_CACHE_DIR"
```

Keep the thin `omnifs` binary in the runtime image only for user-facing container commands that still belong there, such as debug or status helpers. The old Rust builder stages should remain available until the first native release has shipped, but release image assembly should stop using them once native smoke passes.

## Native build jobs

### Preflight fast

Fast preflight should stay small and should not wait for WASI SDK install or Rust target cache hydration. The workflow should invoke the root command surface:

```bash
just ci-preflight-fast
actionlint
zizmor .github/workflows
```

Pin the installers too. Use a pinned `rhysd/actionlint` action, and install `zizmor` through a pinned installer such as `taiki-e/install-action` or `pipx install zizmor==<version>` from `tools/versions.toml`. Do not use floating latest installs in preflight.

### Preflight policy

Policy preflight runs the repository checks that need Bun maintainer scripts:

```bash
just npm-validate
just release-check
```

This job should also enforce that generated platform data stays sourced from `npm/platforms.json` rather than duplicated in workflow YAML.

### WASM

The WASM job runs on Ubuntu and installs:

```text
Rust 1.91.0 from rust-toolchain.toml
wasm32-wasip2 target
WASI_SDK_VERSION=33 with checksum verification
wasm-tools with a pinned version
Swatinem/rust-cache with a wasm shared key
```

Use the release checksum file published with WASI SDK:

```text
https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-33/SHA256SUMS
```

It runs:

```bash
cargo build --release --target wasm32-wasip2 -p 'omnifs-tool-*'
cargo build --release --target wasm32-wasip2 -p 'omnifs-provider-*' -p test-provider
cargo clippy -p 'omnifs-provider-*' -p 'omnifs-tool-*' -p test-provider --target wasm32-wasip2 -- -D warnings
cargo test -p 'omnifs-provider-*' -p test-provider --target wasm32-wasip2 --no-run
wasm-tools validate target/wasm32-wasip2/release/*.wasm
```

It uploads `omnifs-wasm` from `target/wasm32-wasip2/release/*.wasm`.

WASI SDK is removed from the Dockerfile only after this job owns the install and validation path.

### Host verification

Host jobs download `omnifs-wasm` and seed:

```bash
mkdir -p target/wasm32-wasip2/release
cp dist-wasm/*.wasm target/wasm32-wasip2/release/
```

That keeps the current `include_bytes!` contract for `omnifs_tool_archive.wasm` intact while avoiding duplicate tool builds.

Host lint runs:

```bash
cargo clippy -p omnifs-cli -p omnifs-host -p omnifs-sdk \
  -p omnifs-sdk-macros -p omnifs-mount-schema -- -D warnings
```

Host tests should move to `nextest`, but the archive is not sufficient by itself for Omnifs today. Host tests call helpers such as `provider_wasm_path(...)`, which resolve runtime-loaded provider files under `target/wasm32-wasip2/release` from `CARGO_MANIFEST_DIR`. Those files are not compile-time `include_bytes!` inputs for the tests.

The `host-test-build` job must therefore publish both the nextest archive and a WASM sidecar:

```bash
cargo nextest archive --release \
  -p omnifs-cli -p omnifs-host -p omnifs-sdk \
  -p omnifs-sdk-macros -p omnifs-mount-schema \
  --archive-file host-tests.tar.zst

tar -C target/wasm32-wasip2/release \
  -caf host-test-wasm.tar.zst \
  test_provider.wasm omnifs_provider_*.wasm omnifs_tool_*.wasm
```

The `host-test-run` job restores both:

```bash
mkdir -p target/wasm32-wasip2/release
tar -C target/wasm32-wasip2/release -xaf host-test-wasm.tar.zst
cargo nextest run --archive-file host-tests.tar.zst
```

If nextest configuration can include the WASM files inside the archive reliably, that is also acceptable. The invariant is that `host-test-run` must not silently depend on a local `target/` tree from the build job. If runtime dominates after the split, shard only the `nextest run` job. The archive job should be the reusable compile boundary.

### CLI artifacts

`npm/platforms.json` remains the source of truth for platform packages. Native CI builds one CLI artifact per entry. The package target stays the Rust/npm platform identity; the link target is the concrete `cargo-zigbuild` target used to produce the binary.

| Platform package | Package target | Link target | Runner | Builder |
|---|---|---|---|---|
| `linux-x64` | `x86_64-unknown-linux-gnu` | `x86_64-unknown-linux-gnu.2.17` | Ubuntu x64 | `cargo-zigbuild` |
| `linux-arm64` | `aarch64-unknown-linux-gnu` | `aarch64-unknown-linux-gnu.2.17` | Ubuntu x64 | `cargo-zigbuild` |
| `darwin-x64` | `x86_64-apple-darwin` | `x86_64-apple-darwin` | Ubuntu x64 | `cargo-zigbuild` |
| `darwin-arm64` | `aarch64-apple-darwin` | `aarch64-apple-darwin` | Ubuntu x64 | `cargo-zigbuild` |

Use cargo-dist as a build invocation, not as the workflow owner. The workflow remains hand-written because release has custom artifact, Docker, npm, and promotion semantics. `dist generate-ci` must not own `ci.yml`.

Acceptable cargo-dist role if it can package prebuilt artifacts or invoke zigbuild cleanly:

```bash
cargo dist build --target x86_64-apple-darwin
cargo dist build --target aarch64-apple-darwin
```

Fallback packaging role if cargo-dist wants too much workflow ownership:

```bash
cargo zigbuild --release -p omnifs-cli --target x86_64-unknown-linux-gnu.2.17
just ci-package-cli linux-x64 target/.../omnifs
```

Use the same small script path for all four platform tarballs if that is simpler than bending cargo-dist around prebuilt zigbuild outputs. The final shape should have one packaging mechanism for CLI tarballs and one source of truth for platform metadata. Do not let cargo-dist regenerate workflow YAML as part of this.

### Cache strategy

Use `Swatinem/rust-cache` everywhere Cargo runs, including the Darwin cross-build lane. Keep the key count bounded because GitHub Actions cache has a 10 GB repository cap and eviction churn can erase the benefit.

Recommended layout:

```text
rust-host-check:
  shared-key: host-test
  jobs: host-lint, host-test-build, host-test-run

rust-wasm:
  shared-key: wasm32-wasip2
  jobs: wasm

rust-linux-x64:
  shared-key: cli-linux-x64-glibc-2.17
  jobs: cli-linux-x64

rust-linux-arm64:
  shared-key: cli-linux-arm64-glibc-2.17
  jobs: cli-linux-arm64

rust-darwin:
  shared-key: cli-darwin-zigbuild
  jobs: cli-darwin
```

Set `cache-on-failure: true` for Rust caches. This is the failsafe property missing from the current Docker path: if dependencies finish compiling and local code fails, the next run should resume from the compiled dependency state.

## Runtime image assembly

The runtime image jobs should not compile Rust.

For `linux/amd64`:

```text
runner: ubuntu-latest or pinned Ubuntu x64
action: docker/build-push-action@v6
download omnifs-cli-linux-x64 or omnifs-daemon-linux-x64
download omnifs-wasm
extract binary to runtime-context/bin/omnifs
copy provider WASM to runtime-context/providers/
copy runtime scripts to runtime-context/scripts/
docker buildx build --platform linux/amd64 Dockerfile.runtime with labels
push by digest
upload docker-digest-amd64
```

For `linux/arm64`, repeat with `omnifs-cli-linux-arm64` or `omnifs-daemon-linux-arm64` on `ubuntu-24.04-arm`. Do not use QEMU for the release runtime image unless the native arm runner is unavailable.

With Rust gone from Docker, runtime assembly should use `docker/build-push-action@v6` directly. The graph is now one Dockerfile, one context, and two platform-specific jobs.

Wire provenance labels from computed artifact checksums:

```bash
CLI_SHA="$(sha256sum runtime-context/bin/omnifs | awk '{print $1}')"
PROVIDERS_SHA="$(sha256sum dist/checksums/omnifs-SHA256SUMS | awk '{print $1}')"
docker buildx build \
  --label "io.omnifs.cli.sha256=${CLI_SHA}" \
  --label "io.omnifs.providers.sha256=${PROVIDERS_SHA}" \
  --label "org.opencontainers.image.revision=${GITHUB_SHA}" \
  ...
```

The image build now has only OS package and file-copy cache behavior. BuildKit cache refs become small and platform-specific:

```text
buildcache-runtime-base-amd64
buildcache-runtime-base-arm64
buildcache-runtime-assembly-amd64
buildcache-runtime-assembly-arm64
```

There is no `buildcache-deps`, `buildcache-builder`, or `buildcache-providers` because Docker no longer builds Rust.

## Contributor dev workflow

Do not break `omnifs dev` while the release pipeline moves to native artifacts. Contributor setup should not require every developer to install Zig, WASI SDK, and the full release toolchain before they can start the sandbox.

Phase 3 should keep a contributor-only build path:

```text
Dockerfile.runtime:
  release image only
  copies prebuilt native artifacts
  no Rust toolchain

Dockerfile.dev:
  used by omnifs dev
  can keep Rust, cargo-chef, and provider builds while the native release path stabilizes
```

When `Dockerfile.runtime` lands, rename the current release/dev Dockerfile to `Dockerfile.dev` rather than leaving three overlapping entrypoints. Both release and contributor paths should pass `-f` explicitly so plain `docker build .` is not an ambiguous contract.

Keep `Dockerfile.dev` as the contributor path indefinitely unless a later PR proves staged native-artifact dev builds are simpler and at least as fast. That accepts some dev/release Docker divergence to avoid forcing every contributor through the release toolchain.

A later explicit migration could make `omnifs dev` stage artifacts first:

```text
omnifs dev:
  cargo build provider/daemon artifacts locally
  stage runtime-context
  docker build Dockerfile.runtime
```

Do not force that cutover in the pipeline rewrite. Release Docker and contributor Docker have different constraints, and preserving contributor friction is part of the acceptance criteria.

## Release hardening

Native artifacts make hardening stronger because the release files exist before image assembly.

Required evidence:

1. Attest every CLI archive.
2. Attest every provider/tool WASM component.
3. Attest Linux daemon archives separately if the binary split lands.
4. Generate `omnifs-SHA256SUMS` over all release files.
5. Verify checksums before GitHub Release attach.
6. Verify artifact attestations before npm publish.
7. Generate Docker provenance and SBOM for the assembled runtime image on trusted main/release runs.
8. Sign the final multi-platform manifest digest with cosign keyless signing.
9. Scan the final manifest digest.
10. Verify public GitHub Release, npm, and GHCR state after publish.

Sign the manifest-list digest. Verification consumers should reference that multi-platform digest, not the per-platform image digests.

The Docker provenance should include the artifact checksums as labels or annotations where practical:

```text
org.opencontainers.image.revision=<commit>
io.omnifs.cli.sha256=<linux cli sha256>
io.omnifs.daemon.sha256=<linux daemon sha256 if split>
io.omnifs.providers.sha256=<checksum manifest sha256>
```

Cosign public verification must pin both issuer and identity:

```bash
cosign verify ghcr.io/0xff-ai/omnifs@sha256:<digest> \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity-regexp '^https://github.com/0xff-ai/omnifs/\\.github/workflows/(ci|release)\\.yml@refs/heads/main$'
```

Artifact public verification should use GitHub artifact attestations against the expected repository and commit:

```bash
gh attestation verify omnifs-cli-linux-x64.tar.xz \
  --repo 0xff-ai/omnifs \
  --signer-workflow .github/workflows/ci.yml
```

## Workflow security model

Keep `release.yml` as a `workflow_run` consumer of a green `ci.yml` run, but make the trust boundary explicit:

```text
release runs only for successful ci.yml runs
release requires workflow_run.event == push
release requires workflow_run.head_branch == main
release uses workflow_run.head_sha as the release commit
release downloads artifacts only from workflow_run.id
release verifies artifact attestations before publishing
release never runs untrusted pull_request code with write tokens
```

Do not use `pull_request_target` for this pipeline. Release authority belongs to `main` after CI has produced trusted artifacts.

## Supply-chain hardening

These hardening items are orthogonal to native versus Docker builds:

1. Pin third-party actions by full commit SHA.
2. Use Renovate to update pinned actions, Docker base image digests, cargo tools, npm actions, and external download pins.
3. Pin the runtime base image by digest after choosing the Ubuntu release.
4. Add `zizmor` to preflight.
5. Add CODEOWNERS coverage for `.github/`, `Dockerfile*`, `dist-workspace.toml`, `npm/`, `scripts/maint/`, `RELEASING.md`, and release helper scripts.
6. Verify external downloads by checksum, including WASI SDK and any installed cargo tools that are fetched outside Cargo.
7. Keep SBOM and max provenance on trusted main/release paths, not ordinary PRs.
8. Keep public verification commands in the release logs so a user can reproduce them.
9. Add Rust dependency scanning through `cargo deny`; it covers RUSTSEC advisory checks and gives one policy surface for license, source, and advisory rules.

Renovate is the better fit than Dependabot for this PR because it handles action SHA pin refreshes and Docker digest refreshes as one dependency-management policy.

Minimum Renovate deliverable:

```json
{
  "extends": ["config:recommended"],
  "digest": { "enabled": true },
  "github-actions": { "enabled": true },
  "dockerfile": { "enabled": true },
  "customManagers": [
    {
      "customType": "regex",
      "managerFilePatterns": ["/^tools/versions\\.toml$/"],
      "matchStrings": ["# renovate: datasource=(?<datasource>[a-z-]+) depName=(?<depName>[^\\s]+)\\n[^\\s=]+\\s*=\\s*\\\"(?<currentValue>[^\\\"]+)\\\""],
      "datasourceTemplate": "{{datasource}}"
    }
  ]
}
```

Example `tools/versions.toml` entry:

```toml
# renovate: datasource=github-releases depName=rust-cross/cargo-zigbuild
cargo_zigbuild = "0.22.3"
```

## External fixture caching

Two current smoke dependencies are outside the repository and should not be fetched naively on every run:

```text
Chinook_Sqlite.sqlite
ollama/ollama smoke target
```

Cache or mirror these inputs with checksum verification. CI should fail with a clear fixture-fetch error instead of turning external availability into a misleading runtime regression. If a fixture is only needed for smoke, keep it out of basic PR preflight and hydrate it in the smoke lane.

## npm publishing

Release should publish npm from native CLI archives:

```text
download all four omnifs-cli-* artifacts
just npm-sync
just npm-validate
for each platform in npm/platforms.json:
  extract matching archive
  copy omnifs into npm/platform/<platform>/bin/omnifs
  npm pack
  npm publish through Trusted Publishing
publish npm/omnifs root package
verify npm view for root and platform packages
```

Use npm Trusted Publishing and remove `NPM_TOKEN`. The trusted publisher should be scoped to `release.yml`, not all workflows.

Add an npm install/pack verification for the root package and optional platform dependencies. The common failure mode is a stale `optionalDependencies` or lockfile shape that points users at the wrong platform package version even though individual packages were published.

## Workflow changes

### `ci.yml`

Replace Docker Rust lanes with native lanes:

```text
preflight-fast
preflight-policy
wasm
host-lint
host-test-build
host-test-run
verify
cli-linux-x64
cli-linux-arm64
cli-darwin
macos-signing optional
runtime-amd64
runtime-arm64
smoke-amd64
smoke-arm64
docker-publish
```

`cli-*`, `runtime-*`, and optional signing should run only on `main` or trusted `ci-full` PRs. Basic PRs should run `preflight-fast`, `preflight-policy`, `wasm`, `host-lint`, `host-test-build`, `host-test-run`, and `verify`.

`runtime-arm64` runs on `ubuntu-24.04-arm`. `runtime-amd64` runs on Ubuntu x64. Both use `docker/build-push-action@v6`.

Every PR that renames, splits, or deletes CI jobs must update branch protection required-status-checks in the same phase. Otherwise renamed checks can stop gating merges while the new jobs are still running.

### `release.yml`

Keep the current release authority model:

```text
plan -> github-release -> promote -> npm -> public-verify
```

But change the artifact input contract:

```text
Release downloads native CLI artifacts, WASM artifacts, checksum manifest, and digest artifacts from the triggering CI run.
Release never downloads or consumes Docker build scratch outputs.
```

Use official `actions/download-artifact@v8` with `github-token`, `repository`, and `run-id`.

Preserve a guarded `workflow_dispatch` path for transient release retries. Manual runs must require an explicit CI `run-id` and release version, then re-run the same checksum and attestation verification before publishing anything.

```yaml
workflow_dispatch:
  inputs:
    ci_run_id:
      description: CI run ID whose artifacts should be released
      required: true
    release_version:
      description: Unprefixed semver, for example 0.2.0
      required: true
```

### Docker build wiring

Delete Rust build targets from the release path:

```text
deps
lint
test
wasm-artifacts
cli
cli-amd64
cli-arm64
```

Keep only direct runtime image assembly jobs:

```text
runtime-amd64
runtime-arm64
runtime
```

Do not keep a separate Docker graph file for runtime assembly. The release path uses `docker/build-push-action@v6` directly with the staged runtime context.

### `dist-workspace.toml`

Keep cargo-dist configured for package building, not workflow generation.

```text
allowed:
  dist build invocations from hand-written workflow steps

not allowed:
  dist generate-ci owning ci.yml or release.yml
```

If CLI packaging moves deeper into cargo-dist, update `dist-workspace.toml` only after Phase 0 proves glibc, FUSE, and Darwin cross-link compatibility. Keep `allow-dirty = ["ci"]` until there is no custom workflow logic left, which is not the expected outcome for this repo.

## Migration plan

Each phase should ship as a separate PR with its own rollback story. Phase 1 should run on `main` for at least one week before Phase 2 changes the release artifact factory. Do not bundle the Docker boundary rewrite, binary graph split, native artifact publishing, release promotion changes, and supply-chain hardening into one mega-PR.

### Phase 0: ABI, FUSE, and runtime proof spike

1. Capture current CI job timings and artifact sizes.
2. Build `omnifs-wasm` natively on Ubuntu with WASI SDK 33 and `wasm-tools validate`.
3. Build `omnifs-cli-linux-x64` with `cargo-zigbuild --target x86_64-unknown-linux-gnu.2.17`.
4. Build `omnifs-cli-linux-arm64` with `cargo-zigbuild --target aarch64-unknown-linux-gnu.2.17`.
5. Build `omnifs-cli-darwin-x64` and `omnifs-cli-darwin-arm64` from Linux with `cargo-zigbuild`.
6. Pin the working `cargo-zigbuild`, Zig, SDK, and container digest inputs.
7. Run `readelf -V`, `ldd`, and `./omnifs --version` on both Linux artifacts.
8. Run `cargo tree -e features -p omnifs-cli` and `cargo tree -e features -p omnifs-host` to prove the FUSE feature graph.
9. Confirm the separate daemon crate split shape and record any temporary single-binary exception.
10. Verify host tests' runtime-loaded WASM files and prove the nextest WASM sidecar or archive include mechanism.
11. Split preflight into `preflight-fast` and `preflight-policy` in the design before implementation.
12. Test macOS packaged artifacts on a real macOS runner or local machine before deleting native macOS builders.
13. Record the npm/Homebrew-only macOS distribution policy and rust-cross SDK acceptance.
14. Copy the native Linux binary plus provider WASM into temporary runtime images.
15. Run the existing smoke script against image digests. The local Phase 0 helper is `scripts/ci/build-runtime-image.sh`; the smoke harness remains `scripts/ci/smoke-container.sh`.
16. Record the complete Phase 0 deliverables checklist in the PR description before proceeding.

This proves the model before deleting Docker Rust stages.

### Phase 1: native WASM and host verification

1. Add native `wasm` job with WASI SDK 33 checksum verification and `wasm-tools validate`.
2. Split `preflight-fast` from `preflight-policy`.
3. Make host lint/test download and seed `omnifs-wasm`.
4. Move host tests to `nextest archive` and `nextest run` with a WASM sidecar artifact or explicit archive include.
5. Update branch protection required-status-checks for the new preflight and host job names.
6. Run one green dual-track CI pass with old Docker verify and new native verify.
7. Run one green native-only CI pass.
8. Only then remove old Docker verify from the release path.

### Phase 2: native CLI artifacts

The daemon split is the largest Rust-code change in this migration and should start with a small ADR or design note. The boundary should name what stays in the user CLI, what moves to the daemon crate, and which shared library crate, if any, owns common command/session code. Do not make the workflow depend on a vague "feature-gated daemon" split.

1. Split the runtime daemon into a separate crate or binary package that owns `omnifs-host` and `fuser`.
2. Build the user CLI outside Docker with `cargo-zigbuild` and glibc 2.17, without daemon-side dependencies.
3. Build macOS CLI outside macOS runners with `cargo-zigbuild`.
4. Keep cargo-dist as an invoked packager from hand-written workflow steps only if it can package or build through the pinned zigbuild path.
5. Otherwise replace cargo-dist CLI packaging with a small script that packages all four prebuilt binaries from `npm/platforms.json`.
6. Use `cargo metadata --format-version=1` plus structured parsing for provider, tool, and package enumeration instead of shell/awk lists over `providers/*/Cargo.toml` or workflow-local duplicate tables.
7. Add an optional tiny macOS signing/notarization job only if direct download UX becomes supported.
8. Attest all four CLI archives.
9. Update branch protection required-status-checks for CLI artifact jobs before they replace older dist jobs.
10. Generate checksum manifest.

### Phase 3: runtime image assembly

1. Add `Dockerfile.runtime`.
2. Add runtime context staging script.
3. Change `runtime-amd64` and `runtime-arm64` to copy native artifacts.
4. Run `runtime-arm64` on `ubuntu-24.04-arm` and `runtime-amd64` on Ubuntu x64 with `docker/build-push-action@v6`.
5. Preserve `omnifs dev` through `Dockerfile.dev`.
6. Keep smoke before publishing the `sha-*` manifest.
7. Update branch protection required-status-checks for runtime and smoke job names.
8. Keep the old Docker Rust stages available on a fallback branch, or leave them dormant, for one release cycle.
9. Delete Docker Rust targets only after one successful native release and one successful follow-up main run.

### Phase 4: release workflow conversion

1. Replace third-party cross-workflow artifact download with official `actions/download-artifact@v8`.
2. Make GitHub Release attach native CLI/WASM artifacts.
3. Make npm publish consume native CLI artifacts.
4. Switch npm to Trusted Publishing.
5. Add public verification.

### Phase 5: hardening and cleanup

1. Pin third-party actions by full SHA.
2. Add `tools/versions.toml` and Renovate for actions, Docker digests, and tool pins.
3. Add `zizmor`.
4. Add `cargo deny`.
5. Verify external tool downloads by checksum.
6. Pin runtime base image by digest.
7. Remove `cargo-chef`, Rust toolchain setup, WASI SDK, and mold from the release Dockerfile after the native jobs own those responsibilities. The WASI SDK arch-selection branch in the current Dockerfile can be deleted because the native WASM job runs on x64 Linux.
8. Add or document GHCR retention for obsolete build caches and image tags. Default policy should remove untagged cache manifests and keep a bounded set of `sha-*` images, for example the last 20 trusted main builds.
9. Remove obsolete Docker cache refs and GHCR buildcache tags after one successful release.

## Acceptance criteria

The native pipeline is done when:

1. No release-path Docker build stage runs `cargo`.
2. `omnifs-wasm` is produced by native Cargo and consumed by host tests, CLI packaging, npm packaging, and runtime image assembly.
3. `wasm-tools validate` runs on all produced provider and tool components.
4. Four CLI archives are produced as native artifacts and attested before Release.
5. Linux CLI artifacts are linked with glibc 2.17 through `cargo-zigbuild`, and CI fails on newer glibc symbols.
6. The binary graph is split: npm builds a thin CLI without `omnifs-host` or `fuser`, and the runtime image builds a daemon binary that owns host/FUSE dependencies.
7. The Linux runtime images are assembled by copying the matching Linux daemon or CLI binary and provider WASM into the image.
8. The amd64 image is smoked by digest before any `sha-*`, branch, `latest`, or semver manifest tag is published.
9. Release publishes GitHub Release assets and npm packages from the same attested CI artifacts.
10. `NPM_TOKEN` is gone.
11. Docker cache refs contain only runtime image assembly layers, not Rust build artifacts.
12. Darwin artifacts are built from Linux with pinned `cargo-zigbuild`, and unsigned distribution is limited to npm/Homebrew unless a notarization job exists.
13. Host test runs restore runtime-loaded provider WASM files alongside the nextest archive.
14. `preflight-fast` contains no Rust-building work; policy preflight uses Bun maintainer scripts.
15. `runtime-arm64` runs on `ubuntu-24.04-arm`.
16. Runtime image assembly uses `docker/build-push-action@v6` directly.
17. `workflow_dispatch` remains available for guarded release retries by explicit CI run ID.
18. `omnifs dev` still has a contributor-friendly image build path.
19. The PR records before/after CI timing in the PR description and meets the performance targets below, or explains the measured bottleneck and follow-up.
20. Cold trusted factory completes in 14 minutes or less, excluding queue time.
21. Warm trusted factory completes in 6 minutes or less when optional macOS signing is skipped.
22. Basic PR verify completes in under 5 minutes on warm cache.

Performance targets:

```text
cold trusted factory run: 10-14 minutes, excluding queue time
warm trusted factory run: 5-6 minutes if optional macOS signing is skipped
warm trusted factory stretch goal: 3-4 minutes
basic PR verify: under 5 minutes warm cache
```

Expected cold-cache lane budgets:

```text
preflight-fast: under 1 minute
preflight-policy: under 1 minute
wasm: 3-5 minutes
host-test-build plus host-test-run: 4-6 minutes
cli-linux-x64: 5-7 minutes
cli-linux-arm64: 5-7 minutes
cli-darwin: 6-8 minutes
runtime-amd64/runtime-arm64: 1-2 minutes each
smoke-amd64/smoke-arm64: 1-2 minutes each
```

If native CI cannot hit these targets after Phase 1 and Phase 2, measure the slowest lane before introducing runner services or a remote builder.

## Open decisions

1. Temporary FUSE fallback: the final shape is a separate daemon crate; Phase 0 decides only whether a proven pure-Rust `fuser` single binary may survive for one release cycle.
2. CLI archive packager: cargo-dist invocation, small script wrapper, or cargo-dist plus a zig linker environment. Workflow generation remains out of scope.
3. WASM split: build all WASM before host tests, or split a fast `tool-wasm` job from full provider release artifacts.
4. Runtime Dockerfile timing: add `Dockerfile.runtime` first, rename the current Dockerfile to `Dockerfile.dev` in the same phase, then delete old release stages after CI stabilizes.
5. macOS signing and notarization: closed for this plan as npm/Homebrew-only. Reopen only if direct GitHub Release downloads become supported.
6. Apple SDK policy: accepted for this OSS distribution unless project ownership explicitly rejects it before Phase 2.
7. Remote builder: once Rust leaves Docker, Depot becomes less urgent. It may still help runtime image assembly, but the largest win moves to native Cargo caching.

Linux ABI baseline is no longer open. It is glibc 2.17 through `cargo-zigbuild` unless Phase 0 proves that impossible.

## Recommendation

Run Phase 0 immediately. If glibc 2.17 Linux builds pass, the binary works on both host runners and runtime images, Darwin cross-linked artifacts install cleanly, and the FUSE packaging model is settled, implement this native plan instead of deepening the Docker build graph.

If Phase 0 fails on glibc, FUSE linkage, or an incompatible `-sys` crate, pause the native migration and keep `Dockerfile.dev` as the contributor path while designing a narrower fallback. Do not reintroduce a release Docker graph unless a future PR proves it is simpler than direct native artifacts plus runtime image assembly.

## Reference docs

- Docker BuildKit overview: <https://docs.docker.com/build/buildkit/>
- Docker registry cache backend: <https://docs.docker.com/build/cache/backends/registry/>
- GitHub artifact attestations: <https://docs.github.com/en/actions/concepts/security/artifact-attestations>
- GitHub action SHA pinning guidance: <https://docs.github.com/en/enterprise-cloud@latest/actions/how-tos/security-for-github-actions/security-guides/security-hardening-for-github-actions>
- `actions/download-artifact` cross-run inputs: <https://github.com/actions/download-artifact>
- npm Trusted Publishing: <https://docs.npmjs.com/trusted-publishers>
- Sigstore cosign keyless container signing: <https://docs.sigstore.dev/cosign/signing/signing_with_containers/>
- cargo-nextest archive support: <https://nexte.st/docs/ci-features/archiving/>
- `Swatinem/rust-cache`: <https://github.com/Swatinem/rust-cache>
- cargo-zigbuild: <https://github.com/rust-cross/cargo-zigbuild>
- `wasm-tools validate`: <https://github.com/bytecodealliance/wasm-tools>
- WASI SDK releases and checksums: <https://github.com/WebAssembly/wasi-sdk/releases>
- fuser crate features: <https://docs.rs/crate/fuser/latest/features>
- cargo-deny: <https://github.com/EmbarkStudios/cargo-deny>
- cargo-audit: <https://github.com/rustsec/rustsec/tree/main/cargo-audit>
