# Build and validation contracts

Status: current-contract
Owns: local and CI gates, provider build artifacts, generated OpenAPI/schema files, live runtime validation, and documentation checks.

## Read when

Read this before touching CI, `just` recipes, provider artifact generation, wasi-sdk setup, OpenAPI/schema generation, docs checks, runtime smoke paths, or validation guidance.

## Rules

### Provider build artifacts

Provider WASM artifacts are built with the pinned wasi-sdk. `just build providers` compiles providers, then runs the native `omnifs-embed-metadata` harvester, which converts each provider's `Provider::METADATA` const into the host `ProviderManifest` and injects it as the `omnifs.provider-metadata.v1` custom section, and emits `target/omnifs-provider-store` with content-addressed WASM files plus `index.json`. `just dev` runs `scripts/dev.ts`, so dev mount pinning and the dev image both consume the same provider-store bundle.

Provider build and check recipes install the pinned wasi-sdk when needed. Run `just build providers` before host tests that need generated provider artifacts. Use `OMNIFS_ITEST_SKIP_PROVIDER_BUILD=1` after prebuilding providers for nextest runs that would otherwise contend (`just test host` sets it for you).

Provider runtime changes must validate both binding surfaces separately: `omnifs-wit` host bindings with `--features host-bindings`, and SDK/provider guest bindings without that feature. Do not combine those into one Cargo invocation that enables host bindings while compiling the SDK.

Provider component validation must enable the component-model async validation features used by provider exports.

### Generated OpenAPI and schemas

OpenAPI is generated from daemon implementation, and provider manifest schema is generated from provider model types. Keep generated artifacts synchronized with code.

Run `just openapi` after daemon API changes. Run `just schema` after provider manifest schema changes. Keep generated files checked in when their source model changes.

### Live runtime validation

Mount, provider, clone, traversal, frontend, or runtime behavior changes need live runtime validation. Rust checks alone are not enough.

Use `just dev -y` for the supported contributor runtime path. Check status with `omnifs status` directly (host-native, no `docker exec` needed). Exercise shell traversal and real file tools for path-surface changes.

### CI gates

Use the repo gates instead of ad hoc workspace commands. Host-target gates exclude provider/test-provider WASM crates; WASM crates use provider-specific gates.

Run `just check` before a push or PR handoff; it composes formatting, justfile and docs checks, workflow linting, provider checks, host clippy and tests, and whitespace validation. CI keeps those lanes separate for parallelism. Use `just check host` and `just test host` for host-target iteration. Use `just check providers`, `just build providers`, and `just validate providers` for WASM iteration.

### Cross-language facts on the container boundary

The daemon always runs host-native, so `OMNIFS_HOME` and `OMNIFS_MOUNT_POINT` resolve directly from the host environment on every platform. Their name constants and the layout under the home have one owner: `omnifs_workspace::layout::WorkspaceLayout`. The only remaining guest-container path is the optional Docker-hosted FUSE frontend's fixed mount point, `/omnifs`. It is not env-var-driven (the frontend container is credential-free and gets no `OMNIFS_HOME`), so the literal is hardcoded at its owners instead: the frontend image's `ENTRYPOINT` (`Dockerfile`) and each host launcher that targets it (`crates/omnifs-cli/src/launch_backend.rs`'s `GUEST_MOUNT` for production, `scripts/dev.ts`'s `GUEST_MOUNT` for dev). The value is frozen; a change breaks `just dev` and the integration tests loudly.

### Frontend image artifact

Platform CLI archives include the host CLI and its sibling local frontend runners. Linux archives contain `omnifs`, `omnifs-fuse`, and `omnifs-nfs`; Darwin archives contain `omnifs` and `omnifs-nfs`. The matching npm platform package must whitelist the same files, and CI extraction smokes assert every expected executable before running acceptance lanes.

The Docker-hosted FUSE frontend (`omnifs frontend up`) ships a minimal image from `Dockerfile`: `frontend-base` (`debian:trixie-slim`, chosen because Debian's default coreutils/findutils are GNU, which `tail -f` fidelity requires), `frontend-dev` (contributor, built by `just frontend-image`, copies the binary from the `fuse-builder` stage), and `frontend-release` (built by `scripts/ci/build-frontend-image.sh`, injects a prebuilt Linux binary as the `omnifs-fuse-bin` build context). The image runs the slim `omnifs-fuse` binary (`crates/omnifs-fuse/src/bin/omnifs_fuse.rs`, a dedicated `[[bin]]` target — no engine, no Wasmtime, no provider bundle), not the full `omnifs` CLI/daemon binary, so neither stage needs a provider-store build context. The frontend image carries no launch-protocol/min-launcher-version label: `DockerBackend::launch` (`crates/omnifs-cli/src/frontend_backend.rs`) starts the container and checks its credential-free shape without consulting such a label.

CI builds and pushes the frontend image per architecture in the PR lane (`frontend-amd64`/`frontend-arm64`), smokes it directly with `scripts/ci/smoke-frontend-image.sh` (version, GNU `tail`, fails loudly with no `OMNIFS_ATTACH_ADDR`), and on a `main` push merges the per-arch digests into one multi-platform manifest via `scripts/ci/publish-manifest.sh`. Release promotes that manifest to `ghcr.io/0xff-ai/omnifs-frontend:<version>` through `scripts/ci/promote-image.sh`. The `fuse-docker` job (needs `frontend-amd64`'s image digest and the packaged Linux CLI, mirroring `conformance-fuse`'s input shape) runs `crates/omnifs-itest/tests/frontend_docker` against a live host-native daemon and the real amd64 image: the `fuse-docker` conformance column, `omnifs frontend {up,down,status}` lifecycle, `omnifs down` teardown ordering, a cold-start budget, cross-mount byte identity, kill/reattach behavior, and the no-credentials contract. Its scorecards upload as `conformance-scorecards-fuse-docker`, next to `conformance-fuse`'s own artifact.

### Guest disk image artifact (libkrun driver)

The krunkit driver's guest ships as a bootable raw disk image, not a container: `scripts/guest-image/` holds an `mkosi` project (`mkosi/mkosi.conf` plus `mkosi/mkosi.extra/` for the systemd units and tmpfiles rules) that assembles a minimal Debian trixie arm64 EFI image (systemd-boot, fuse3, dropbear-bin, no cloud-init). `just guest-image` (`scripts/guest-image/build.sh`) extracts the linux/arm64 `omnifs-fuse` binary from the same shared `fuse-builder` Dockerfile stage the frontend image uses (or reuses an already-built one passed via `OMNIFS_FUSE_BIN`, which CI does), then runs `mkosi` inside a privileged container (`scripts/guest-image/builder.Dockerfile`, since mkosi needs Linux loop devices and `systemd-repart` that macOS lacks) to bake it in at `/usr/local/bin/omnifs-fuse`. No provider-store bundle is needed: `omnifs-fuse` needs no engine or Wasmtime, unlike the full `omnifs` CLI/daemon binary.

Root login is split into two `mkosi` profiles selected by `--profile` (`build.sh`'s passthrough, or `GUEST_IMAGE_PROFILE`), via `mkosi.profiles/{dev,release}/mkosi.conf`: `dev` (the `just guest-image` default) keeps an unlocked, autologin-enabled root console for the boot smoke and manual debugging; `release` sets neither `RootPassword=` nor `Autologin=`, so root has no password login (mkosi never touches `/etc/shadow` when `RootPassword=` is unset, leaving Debian's own locked default) and no getty unit autologins. `scripts/ci/check-guest-image.sh IMAGE_PATH {dev|release}` asserts the built image's static shape — fail-closed, non-zero exit on any violation — by loop-mounting it read-only inside a throwaway privileged container (works identically on macOS and Linux, since loop-mounting a GPT image needs kernel facilities macOS lacks natively): `/usr/local/bin/omnifs-fuse` present and executable; all six `omnifs-*` units present, with the three that declare `[Install]` (`omnifs-seed-mount.service`, `omnifs-frontend.service`, `omnifs-ssh-setup.service`) enabled; no cloud-init anywhere; and, for `release` only, the locked `/etc/shadow` root entry and the absence of the three autologin drop-ins (`console-getty.service.d`, `getty@tty1.service.d`, `serial-getty@hvc0.service.d`). It is runnable locally against either profile's build output, not just in CI.

Attach parameters (`OMNIFS_ATTACH_ADDR`, `OMNIFS_ATTACH_TOKEN`, `OMNIFS_READY_VSOCK_PORT`, `OMNIFS_SSH_PUBKEY`) reach the guest through a per-launch seed ISO, not cloud-init: `KrunkitBackend::launch` (`crates/omnifs-cli/src/krunkit_backend.rs`) builds an ISO9660+Joliet volume labeled `OMNIFS-SEED` with `hdiutil makehybrid`, auditing the staging directory against the exact expected key set before burning it (only the attach token among them is sensitive). The guest's `omnifs-seed-mount.service` mounts it by label before `omnifs-frontend.service` and `omnifs-ssh-setup.service` source it via `EnvironmentFile=`/a plain read. A missing seed volume or config file fails both units loudly in the journal; neither hangs silently, and an omitted `OMNIFS_SSH_PUBKEY` leaves the guest's vsock ssh socket un-started (logged, not silent) rather than accepting into a guest with no `authorized_keys`. `scripts/guest-image/make-seed-iso.sh` is the standalone bash equivalent `just guest-image-smoke` (`scripts/guest-image/smoke.sh`) uses to boot the image under `krunkit` with a throwaway seed carrying an unreachable placeholder address (and no ssh key), checking the serial console log for the guest reaching `multi-user.target` and `omnifs-frontend.service` starting.

The krunkit BOOT smoke (`just guest-image-smoke`) and the krunkit conformance lane are both local-only gates: GitHub-hosted runners cannot nest virtualization, so neither runs in CI. Run them yourself before landing a change that touches guest boot behavior, the seed protocol, or the krunkit driver.

CI builds the guest image on a native arm64 runner (`guest-image-arm64` in `ci.yml`, gated by `scripts/guest-image/**`/`crates/omnifs-fuse/**`/`crates/omnifs-vfs-wire/**` path changes or a push to `main`): it consumes the `fuse-linux-arm64` job's binary artifact, builds the `release` profile, runs `check-guest-image.sh release` against it, compresses the result with `zstd -19`, and pushes it as an OCI artifact (`oras push`, artifact type `application/vnd.omnifs.guest-image.v1+zstd`, one blob) to `ghcr.io/0xff-ai/omnifs-guest:sha-<commit>`. `oras` is a CI-only tool; it is never a CLI or product dependency. A fork PR builds and asserts the image but skips the push with a loud warning (no registry write access from a fork's `GITHUB_TOKEN`). On ship, `release.yml`'s `promote` job retags the sha-keyed artifact to the version (`scripts/ci/promote-guest-image.sh`, mirroring `promote-image.sh`'s wait-for-artifact retry loop but using `oras tag` instead of `docker buildx imagetools create`, since the guest image is a single-arch non-container artifact) and attests its provenance, exactly like the frontend image.

The CLI's krunkit driver mirrors the frontend image's channel split (`GuestImageSource::resolve` in `crates/omnifs-cli/src/krunkit_backend.rs`): a release build defaults to `ghcr.io/0xff-ai/omnifs-guest:<version>` and pulls it on first use via `crate::guest_image_pull` (plain `reqwest`, not `oras`: anonymous ghcr token, manifest fetch accepting both the OCI image manifest and legacy artifact manifest media types, blob fetch, sha256 verification against the manifest before the file is trusted, cached under `<cache_dir>/guest-images/`); a dev build never downloads and defaults to the local `target/guest-image/omnifs-guest.raw`, naming `just guest-image` in its not-found error.

### Krunkit conformance lane (local-only, never CI)

`crates/omnifs-itest/tests/frontend_krunkit` runs the `fuse-krunkit` conformance column (the same shared row table and scorecard machinery `tests/frontend_docker` uses for the Docker-hosted frontend) against a live krunkit guest: `omnifs up --no-frontend`, `omnifs frontend up --driver krunkit`, the matrix over ssh-over-vsock via `omnifs shell -- <cmd>`, then `omnifs down` with a teardown-cleanliness assertion (no leftover krunkit process, pidfile, or socket). Run it with `just krunkit-conformance` (builds the guest image first if missing, then sets `OMNIFS_ACCEPTANCE_LIVE=1` and runs the suite). Gated on `cfg(target_os = "macos")` plus the `OMNIFS_ACCEPTANCE_LIVE` opt-in, mirroring the live NFS lanes' skip-not-pass convention, and serialized against every other live-mount lane through this crate's one cross-process lock (`omnifs_itest::live::nfs_serial_lock`).

This lane can **never** run in GitHub-hosted CI: krunkit boots a libkrun microVM, and GitHub's hosted macOS runners do not support nested virtualization. It stays a declared local-only gate a contributor runs by hand before a krunkit-affecting change, not a lane that silently skips in CI and reads green.

### Documentation checks

`just docs-check` verifies doc-to-doc links and the contract file template. It does not validate code symbols or code paths. It is a local convenience recipe only; CI does not run it, so it never blocks a merge.

## Must not

- Treat missing provider WASM in a fresh worktree as a product regression.
- Use `cargo check --workspace --all-targets` as a host gate.
- Treat host-target provider checks as proof the metadata section was injected; only `just build providers` runs the harvester that injects it.
- Hand-edit generated OpenAPI or schema files as the primary fix.
- Change API/model code without regenerating the corresponding checked-in artifact and running its focused parity test.
- Validate only the intended leaf path when parent traversal changed.
- Treat compile-time route validity as enough for seal-time behavior.
- Ignore runtime logs when the mount returns `Input/output error`.
- Treat a local aggregate command as the source of truth when CI runs the lanes directly.
- Run host tests that rebuild providers in parallel without prebuilding providers when contention matters.
- Treat `just docs-check` as code-symbol validation.
- Reintroduce a second copy of the frontend apt block; edit `frontend-base` instead.
- Add a fourth literal for the frontend's fixed `/omnifs` guest mount point instead of updating its three existing owners together.
- Give the frontend image an `OMNIFS_HOME` or a provider store. It only ever runs `omnifs-fuse`.
- Push the guest image to ghcr from a contributor machine; only the `guest-image-arm64` CI job and `release`'s `promote` job do that.
- Weaken `check-guest-image.sh`'s release-profile assertions to make a build pass instead of fixing the image.
- Expect `crates/omnifs-itest/tests/frontend_krunkit` to ever run in GitHub-hosted CI, or weaken its skip-when-not-opted-in behavior into a silent pass.

## Code

- `just/dev.just`
- `just/npm.just`
- `scripts/ci/build-providers.sh`
- `npm/package.json`
- `scripts/ci/check-doc-links.sh`
- `scripts/ci/check-doc-contracts.sh`
- `crates/omnifs-daemon/src/bin/openapi.rs`
- `crates/omnifs-api/openapi/daemon.json`
- `crates/omnifs-workspace/schema/omnifs.provider.schema.json`
- `crates/omnifs-itest/src/lib.rs`
- `crates/omnifs-itest/src/matrix.rs`
- `crates/omnifs-itest/tests/frontend_krunkit/main.rs`
- `crates/omnifs-cli/src/provider_bundle.rs`
- `Dockerfile`
- `scripts/ci/common.sh`
- `scripts/ci/build-frontend-image.sh`
- `scripts/ci/smoke-frontend-image.sh`
- `scripts/ci/publish-manifest.sh`
- `scripts/ci/promote-image.sh`
- `scripts/ci/check-guest-image.sh`
- `scripts/ci/promote-guest-image.sh`
- `scripts/guest-image/build.sh`
- `scripts/guest-image/mkosi/mkosi.profiles/dev/mkosi.conf`
- `scripts/guest-image/mkosi/mkosi.profiles/release/mkosi.conf`
- `crates/omnifs-cli/src/krunkit_backend.rs`
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
- `just openapi`
- `just docs-check`
- `just krunkit-conformance` (macOS Apple Silicon only, local-only, never CI: see "Krunkit conformance lane" above)

Live runtime path (the daemon runs host-native; only the frontend needs `docker exec`):

```bash
just dev -y
omnifs status
FRONTEND=$(docker ps --filter label=ai.0xff.omnifs.home="$HOME/.omnifs-dev" --format '{{.Names}}')
docker exec -it -w /omnifs "$FRONTEND" /bin/sh
tail -n 80 ~/.omnifs-dev/cache/daemon.log
```

Frontend image, built standalone (no daemon, no attach):

```bash
just frontend-image
docker run --rm --entrypoint /usr/local/bin/omnifs-fuse omnifs-frontend:dev --version
docker run --rm --entrypoint tail omnifs-frontend:dev --version | head -1
docker run --rm omnifs-frontend:dev # fails loudly: OMNIFS_ATTACH_ADDR is unset
```

Guest image, both `mkosi` profiles plus the krunkit boot smoke (local-only; `just guest-image-smoke` requires `krunkit` on `PATH`, and the krunkit conformance lane needs the same):

```bash
just guest-image
scripts/ci/check-guest-image.sh target/guest-image/omnifs-guest.raw dev
GUEST_IMAGE_PROFILE=release OUT_DIR=target/guest-image-release scripts/guest-image/build.sh
scripts/ci/check-guest-image.sh target/guest-image-release/omnifs-guest.raw release
just guest-image-smoke
```
