# Build and validation contracts

Status: current-contract
Owns: local and CI gates, provider build artifacts, generated schema files, live runtime validation, and documentation checks.

## Read when

Read this before touching CI, `just` recipes, provider artifact generation, wasi-sdk setup, schema generation, docs checks, runtime smoke paths, or validation guidance.

## Rules

### Provider build artifacts

Provider WASM artifacts are built with the pinned wasi-sdk. `just build providers` compiles providers whose macros emit one `omnifs.provider-metadata.v1` custom section directly into each component, then emits `target/omnifs-provider-store` with content-addressed WASM files plus `index.json`. `just dev` runs `scripts/dev.ts`, so dev mount pinning and the dev image both consume the same provider-store bundle.

Provider build and check recipes install the pinned wasi-sdk when needed. Run `just build providers` before host tests that need generated provider artifacts. Use `OMNIFS_ITEST_SKIP_PROVIDER_BUILD=1` after prebuilding providers for nextest runs that would otherwise contend (`just test host` sets it for you).

Host integration fixtures keep runtime data private but share Wasmtime's content-addressed compiled-component cache through `omnifs_engine::test_support::wasm_cache_dir`. CI runs nextest directly from its dedicated host target directory and caches the exact compiled-component directory selected beneath it; caching Wasmtime's global default does not accelerate fixtures whose `HostContext` selects another path. Do not archive and re-extract the test binaries in the same job: the archive duplicates several gigabytes of statically linked executables, increases peak disk use, and adds no transfer boundary.

Provider runtime changes must validate both binding surfaces separately: `omnifs-wit` host bindings with `--features host-bindings`, and SDK/provider guest bindings without that feature. Do not combine those into one Cargo invocation that enables host bindings while compiling the SDK.

Provider component validation must enable the component-model async validation features used by provider exports.

### Generated schemas

The provider manifest schema is generated from provider model types. The local control protocol is handwritten typed wire code in `omnifs-api`, so it has no generated control-plane artifact. Keep the provider schema synchronized with its source model.

Run `just schema` after provider manifest schema changes. Keep the generated provider schema checked in when its source model changes.

The control protocol has one current version in every request and reply envelope. `DaemonStatus` retains operational nested types consumed by Inventory, and `FilesystemInfo.mount_point` remains the per-filesystem wire field. Protocol tests exercise the typed request/reply shapes directly.

### Live runtime validation

Mount, provider, clone, traversal, filesystem, or runtime behavior changes need live runtime validation. Rust checks alone are not enough.

Use `just dev -y` for the supported contributor runtime path. Check status with `omnifs status` directly (host-native, no `docker exec` needed). Exercise shell traversal and real file tools for path-surface changes.

### CI gates

Use the repo gates instead of ad hoc workspace commands. Host-target gates exclude provider/test-provider WASM crates; WASM crates use provider-specific gates.

Run `just check` before a push or PR handoff; it composes formatting, justfile and docs checks, workflow linting, provider checks, host clippy and tests, and whitespace validation. CI keeps those lanes separate for parallelism. Use `just check host` and `just test host` for host-target iteration. Use `just check providers`, `just build providers`, and `just validate providers` for WASM iteration.

### Cross-language facts on the container boundary

The daemon always runs host-native, so `OMNIFS_HOME` resolves from the host environment on every platform. `omnifs_workspace::Workspace` owns that resolution and all derived paths. Guest runtimes use the fixed location `/omnifs`; `fs::Spec` stores it, launchers pass it through `--location`, and `scripts/dev.ts` uses the same value. The image entrypoint contains only `/usr/local/bin/omnifs-thin`; the launcher supplies the flat ID, protocol, runtime, and location arguments.

### Filesystem image artifact

Linux and Darwin x64 CLI archives include exactly `omnifs` and the sibling `omnifs-thin` runner. Linux thin supports `fuse` and `nfs`; Darwin thin supports `nfs`. Darwin arm64 also carries `omnifs-libkrun` plus `libexec/omnifs/{libkrun.1.dylib,KRUN_EFI.silent.fd,runtime-manifest.json,licenses/}`. The matching npm platform package must whitelist the same files, and CI extraction smokes assert every expected executable before running acceptance lanes.

CI has one authoritative Linux `omnifs-thin` producer per architecture. CLI packaging consumes that binary together with the separately built full CLI, while filesystem and guest-image jobs consume the same artifact. Darwin x64 cross-links on Linux. Darwin arm64 builds on the standard native `macos-15` Apple Silicon runner, builds pinned libkrun 1.19.4 from revision `728df8125077d0db44265f6e997c72b81b65c015` with only its EFI feature set, stages the pinned EFI firmware and license sources, rejects GPU and forbidden dynamic links, and applies an ad hoc CI signature so the payload can be checked. CI never boots the guest because hosted runners do not support nested virtualization.

Release replaces the CI Darwin arm64 archive with the same payload signed under one Developer ID team. It signs the dylib before the helper, grants only the Hypervisor entitlement to `omnifs-libkrun`, submits one zip to Apple's notary service, records that submission ID, and polls the same submission in a later job. GitHub and npm publication cannot start until the status is `Accepted`; rejected, invalid, missing, or timed-out submissions fail the release. The final archive comes from the signed payload saved before submission, so polling never rebuilds, resigns, or resubmits it.

The Docker-hosted FUSE filesystem ships a minimal image from `Dockerfile`: `filesystem-base`, `filesystem-dev`, and `filesystem-release`. The image runs the flat `omnifs-thin` interface with no engine runtime, Wasmtime, or provider bundle, so neither stage needs a provider-store build context. The launcher supplies `--name`, `--protocol`, `--runtime`, and `--location`, while `OMNIFS_ATTACH_ADDR` remains the only Omnifs launch env variable.

CI builds and pushes the filesystem image per architecture in the PR lane (`filesystem-amd64`/`filesystem-arm64`), smokes it directly with `scripts/ci/smoke-filesystem-image.sh`, and on a `main` push merges the per-arch digests into one multi-platform manifest. The `fuse-docker` job runs `crates/omnifs-itest/tests/filesystem_docker` against a live host-native daemon and the real image: named `fs create|attach|detach|restart|ls` lifecycle, `omnifs down` ordering, cold start, cross-mount byte identity, kill/reattach behavior, and the no-credentials contract.

### Guest disk image artifact (libkrun runtime)

The libkrun runtime's guest ships as a bootable raw disk image, not a container: `scripts/guest-image/` holds an `mkosi` project (`mkosi/mkosi.conf` plus `mkosi/mkosi.extra/` for the systemd units and tmpfiles rules) that assembles a minimal Debian trixie arm64 EFI image (systemd-boot, fuse3, dropbear-bin, no cloud-init). `just guest-image` (`scripts/guest-image/build.sh`) extracts the linux/arm64 `omnifs-thin` binary from the shared `thin-builder` Dockerfile stage (or reuses one passed via `OMNIFS_THIN_BIN`), then runs `mkosi` inside a privileged container to bake it in at `/usr/local/bin/omnifs-thin`. No provider-store bundle is needed: `omnifs-thin` needs no engine runtime or Wasmtime, unlike the full `omnifs` CLI/daemon binary.

Root login is split into two `mkosi` profiles selected by `--profile` (`build.sh`'s passthrough, or `GUEST_IMAGE_PROFILE`), via `mkosi.profiles/{dev,release}/mkosi.conf`: `dev` (the `just guest-image` default) keeps an unlocked, autologin-enabled root console for the boot smoke and manual debugging; `release` sets neither `RootPassword=` nor `Autologin=`, so root has no password login (mkosi never touches `/etc/shadow` when `RootPassword=` is unset, leaving Debian's own locked default) and no getty unit autologins. `scripts/ci/check-guest-image.sh IMAGE_PATH {dev|release}` asserts the built image's static shape — fail-closed, non-zero exit on any violation — by loop-mounting it read-only inside a throwaway privileged container (works identically on macOS and Linux, since loop-mounting a GPT image needs kernel facilities macOS lacks natively): `/usr/local/bin/omnifs-thin` present and executable; all six `omnifs-*` units present, with the three that declare `[Install]` (`omnifs-seed-mount.service`, `omnifs-filesystem.service`, `omnifs-ssh-setup.service`) enabled; no cloud-init anywhere; and, for `release` only, the locked `/etc/shadow` root entry and the absence of the three autologin drop-ins (`console-getty.service.d`, `getty@tty1.service.d`, `serial-getty@hvc0.service.d`). It is runnable locally against either profile's build output, not just in CI.

Attach parameters (`OMNIFS_FS_ID`, `OMNIFS_ATTACH_ADDR`, `OMNIFS_READY_VSOCK_PORT`, `OMNIFS_SSH_PUBKEY`) reach the guest through a per-launch seed ISO, not cloud-init. `LibkrunRunner::launch` builds an ISO9660+Joliet volume labeled `OMNIFS-SEED` and audits the exact key set before burning it. The guest services source that seed and invoke flat `omnifs-thin --name ... --protocol fuse --runtime libkrun --location /omnifs` arguments. Missing identity or attach data fails loudly.

The libkrun BOOT smoke (`just guest-image-smoke`) and the libkrun conformance lane are both local-only gates: GitHub-hosted runners cannot nest virtualization, so neither runs in CI. Run them yourself before landing a change that touches guest boot behavior, the seed protocol, or the libkrun runtime.

CI builds the guest image on a native arm64 runner (`guest-image-arm64` in `ci.yml`, gated by `scripts/guest-image/**`, `crates/omnifs-thin/**`, both protocol crates, `crates/omnifs-vfs/**`, or a push to `main`): it consumes the `thin-linux-arm64` job's binary artifact, builds the `release` profile, runs `check-guest-image.sh release` against it, compresses the result with `zstd -19`, and pushes it as an OCI artifact (`oras push`, artifact type `application/vnd.omnifs.guest-image.v1+zstd`, one blob) to `ghcr.io/0xff-ai/omnifs-guest:sha-<commit>`. `oras` is a CI-only tool; it is never a CLI or product dependency. A fork PR builds and asserts the image but skips the push with a loud warning (no registry write access from a fork's `GITHUB_TOKEN`). On ship, `release.yml`'s `promote` job retags the sha-keyed artifact to the version (`scripts/ci/promote-guest-image.sh`, mirroring `promote-image.sh`'s wait-for-artifact retry loop but using `oras tag` instead of `docker buildx imagetools create`, since the guest image is a single-arch non-container artifact) and attests its provenance, exactly like the filesystem image.

The CLI's libkrun runtime mirrors the filesystem image's channel split (`resolve_guest_image` in `crates/omnifs-cli/src/libkrun_runner.rs`): a release build defaults to `ghcr.io/0xff-ai/omnifs-guest:<version>` and pulls it on first use via `crate::guest_image_pull` (plain `reqwest`, not `oras`: anonymous ghcr token, manifest fetch accepting both the OCI image manifest and legacy artifact manifest media types, blob fetch, sha256 verification against the manifest before the file is trusted, cached under `<cache_dir>/guest-images/`); a dev build never downloads and defaults to the local `target/guest-image/omnifs-guest.raw`, naming `just guest-image` in its not-found error.

### Libkrun conformance lane (local-only, never CI)

`crates/omnifs-itest/tests/filesystem_libkrun` runs the `fuse-libkrun` conformance column against a live guest: it creates and attaches `itest-libkrun`, runs the matrix through `omnifs fs shell --name itest-libkrun -- <cmd>`, and proves detach cleanliness. Run it with `just libkrun-conformance`; it remains a local-only, opt-in lane serialized with other live mount tests.

This lane can **never** run in GitHub-hosted CI: libkrun boots a libkrun microVM, and GitHub's hosted macOS runners do not support nested virtualization. It stays a declared local-only gate a contributor runs by hand before a libkrun-affecting change, not a lane that silently skips in CI and reads green.

### Documentation checks

`just docs-check` verifies doc-to-doc links and the contract file template. It does not validate code symbols or code paths. It is a local convenience recipe only; CI does not run it, so it never blocks a merge.

## Must not

- Treat missing provider WASM in a fresh worktree as a product regression.
- Use `cargo check --workspace --all-targets` as a host gate.
- Treat host-target provider checks as proof the metadata section was injected; only `just build providers` runs the harvester that injects it.
- Hand-edit generated schema files as the primary fix.
- Change provider model code without regenerating the corresponding checked-in schema and running its focused schema test.
- Validate only the intended leaf path when parent traversal changed.
- Treat Rust type-checking as enough for `Router::compile` behavior.
- Ignore runtime logs when the mount returns `Input/output error`.
- Treat a local aggregate command as the source of truth when CI runs the lanes directly.
- Run host tests that rebuild providers in parallel without prebuilding providers when contention matters.
- Treat `just docs-check` as code-symbol validation.
- Reintroduce a second copy of the filesystem apt block; edit `filesystem-base` instead.
- Add a fourth literal for the filesystem's fixed `/omnifs` guest mount point instead of updating its three existing owners together.
- Give the filesystem image an `OMNIFS_HOME` or a provider store. It only ever runs `omnifs-thin --protocol fuse`.
- Push the guest image to ghcr from a contributor machine; only the `guest-image-arm64` CI job and `release`'s `promote` job do that.
- Weaken `check-guest-image.sh`'s release-profile assertions to make a build pass instead of fixing the image.
- Expect `crates/omnifs-itest/tests/filesystem_libkrun` to ever run in GitHub-hosted CI, or weaken its skip-when-not-opted-in behavior into a silent pass.

## Code

- `just/dev.just`
- `just/npm.just`
- `scripts/ci/build-providers.sh`
- `npm/package.json`
- `scripts/ci/check-doc-links.sh`
- `scripts/ci/check-doc-contracts.sh`
- `crates/omnifs-api/src/control.rs`
- `crates/omnifs-workspace/schema/omnifs.provider.schema.json`
- `crates/omnifs-itest/src/lib.rs`
- `crates/omnifs-itest/src/matrix.rs`
- `crates/omnifs-itest/tests/filesystem_libkrun/main.rs`
- `crates/omnifs-cli/src/provider_bundle.rs`
- `Dockerfile`
- `scripts/ci/common.sh`
- `scripts/ci/build-filesystem-image.sh`
- `scripts/ci/smoke-filesystem-image.sh`
- `scripts/ci/publish-manifest.sh`
- `scripts/ci/promote-image.sh`
- `scripts/ci/check-guest-image.sh`
- `scripts/ci/promote-guest-image.sh`
- `scripts/ci/build-libkrun-runtime.sh`
- `scripts/ci/check-libkrun-runtime.sh`
- `scripts/ci/check-darwin-arm64-payload.sh`
- `scripts/ci/sign-darwin-arm64-payload.sh`
- `scripts/ci/wait-for-notarization.sh`
- `scripts/guest-image/build.sh`
- `scripts/guest-image/mkosi/mkosi.profiles/dev/mkosi.conf`
- `scripts/guest-image/mkosi/mkosi.profiles/release/mkosi.conf`
- `crates/omnifs-cli/src/libkrun_runner.rs`
- `crates/omnifs-libkrun/src`
- `crates/omnifs-cli/src/guest_image_pull.rs`
- `CONTRIBUTING.md`

## Validation

- `just check`
- `just build providers`
- `just check providers`
- `just validate providers`
- `just check host`
- `just test host`
- `just refresh`
- `just schema`
- `just docs-check`
- `just libkrun-runtime` (macOS Apple Silicon only; stages the pinned private helper payload under `target/debug`)
- `just libkrun-conformance` (macOS Apple Silicon only, local-only, never CI: see "Libkrun conformance lane" above)

Live runtime path (the daemon runs host-native; only the filesystem needs `docker exec`):

```bash
just dev -y
target/debug/omnifs status
FILESYSTEM=$(docker ps --filter label=ai.0xff.omnifs.home="$HOME/.omnifs-dev" --format '{{.Names}}')
docker exec -it -w /omnifs "$FILESYSTEM" /bin/sh
tail -n 80 ~/.omnifs-dev/cache/daemon.log
```

Filesystem image, built standalone (no daemon, no attach):

```bash
just filesystem-image
docker run --rm --entrypoint /usr/local/bin/omnifs-thin omnifs-filesystem:dev --version
docker run --rm --entrypoint tail omnifs-filesystem:dev --version | head -1
docker run --rm omnifs-filesystem:dev # fails loudly: OMNIFS_ATTACH_ADDR is unset
```

Guest image, both `mkosi` profiles plus the libkrun boot smoke (local-only; `just guest-image-smoke` and the conformance lane build the private runtime through `just libkrun-runtime`):

```bash
just guest-image
scripts/ci/check-guest-image.sh target/guest-image/omnifs-guest.raw dev
GUEST_IMAGE_PROFILE=release OUT_DIR=target/guest-image-release scripts/guest-image/build.sh
scripts/ci/check-guest-image.sh target/guest-image-release/omnifs-guest.raw release
just guest-image-smoke
```
