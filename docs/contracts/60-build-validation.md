# Build and validation contracts

Status: current-contract
Owns: local and CI gates, build artifacts, generated schemas, live validation,
and documentation checks.

## Read when

Read this before changing CI, `just`, provider artifacts, wasi-sdk, schemas,
docs checks, runtime smoke paths, or validation guidance.

## Rules

### Provider build artifacts

Provider recipes install the pinned wasi-sdk. `just build providers` emits
metadata-bearing components and `target/omnifs-provider-store`, a
content-addressed store plus `index.json` consumed by `just dev`. Host test
helpers reuse the built components and only invoke `just build providers` when
the requested artifact is missing. A target-directory lock serializes the
first build when several test binaries start together, so callers do not need
an environment switch.

Host fixtures isolate runtime state but share the explicit compiled-component
cache from `wasm_cache_dir`. CI caches that selected directory under its host
target, not Wasmtime's global default. Do not archive and re-extract test
binaries within a job; it adds no transfer boundary and duplicates gigabytes.

`omnifs-wit` keeps guest `provider` and feature-gated host `host` bindings as
coexisting modules, never feature alternatives. SDK/providers use the guest;
engine/itest use the host. Validate both because they use different bindgen
stacks and targets.

Provider component validation enables the component-model async features used
by provider exports.

### Generated schemas

Provider model types generate the checked-in manifest schema; run `just schema`
when they change. The checked-in control proto uses vendored `protoc` to emit
Rust into Cargo output; generated control Rust is not checked in.

The sole control package is `omnifs.control.v1`, without a postcard frame or
separate version prefix. Unary and stream items cap at 1 MiB; log tails cap at
10,000 lines. Tests cover plan/apply, initial snapshots, terminal events,
receipt recovery, Filesystem identity, and typed conversions.

### Shared wire and API migrations

A WIT, protobuf, VFS wire, shared domain type, or public constructor change is
one migration. Update its definition, bindings, constructors, patterns,
callers, fixtures, and boundary tests together.

Validate the migration in dependency order:

1. Check the crate that owns the changed type or schema.
2. Check its nearest host and guest consumers.
3. Run the focused wire or protocol tests.
4. Check the daemon and CLI surfaces.
5. Run the broad host gate.

After WIT changes, rebuild guest and host bindings. If Wasmtime output stays
stale, clean only `omnifs-wit`. Never use a workspace-wide failure list as the
migration inventory.

### KCL boundary

`omnifs-kcl` uses the official in-process API at one pinned revision, never a
subprocess. Tests prove strict output decoding, local provider digest checks, no
implicit remote fetch, and typed Rust conversion. Temporary KCL JSON contains
no secrets.

Rust declarations and SQLite own the resource contract. Add no KCL schema or
Rust generated from KCL; any typed client definitions derive from Rust.

Keep upstream KCL behind `omnifs-kcl`. Revision changes rerun strict decode,
import-boundary, and release-target checks. Remote modules remain gated.

### Live runtime validation

Mount, provider, clone, traversal, filesystem, or runtime changes need live
validation; Rust checks alone are insufficient.

Use `just dev -y`; it applies rendered KCL, waits for terminal revision, then
opens `fs shell dev-docker`. Check host-native status directly and use
real traversal and file tools for path changes.

### CI gates

Use repository gates, not ad hoc workspace commands. Host gates exclude WASM
provider crates; provider gates target them. `just check` is the pre-push or
handoff aggregate. For quick local feedback, `just test fast` runs the
library and binary-target tests through the `nextest` fast profile. It does
not cover integration or live targets. Iterate with `just check host` and
`just test host`; use `just check providers`, `just build providers`, and
`just validate providers` for WASM.

Control-plane tests cover fast apply, compare-and-swap, durable receipts,
bounded progress and resync, cold-cache preparation with the shared engine, and
Credential or Filesystem action recovery. Runtime changes also need host,
Docker, multi-filesystem, down/restore, and live deletion lanes. Run libkrun
only on an opted-in Apple Silicon host.

### Cross-language facts on the container boundary

The daemon always runs host-native, so host `OMNIFS_HOME` selects one `Profile`
for logging, control, identity, and `DaemonStatePaths`; bootstrap identity covers
only pre-RPC safety. Daemon state lives under `<profile>/daemon-state`. Guests
use `/omnifs` and the sole image entrypoint `/usr/local/bin/omnifs-thin`;
launchers supply flat Filesystem identity arguments.

### Filesystem image artifact

| Archive | Required payload |
|---|---|
| Linux | `omnifs`, `omnifs-thin` with FUSE and NFS |
| Darwin x64 | `omnifs`, `omnifs-thin` with NFS |
| Darwin arm64 | `omnifs`, `omnifs-thin` with NFS, `omnifs-libkrun`, and `libexec/omnifs/{libkrun.1.dylib,KRUN_EFI.silent.fd,runtime-manifest.json,licenses/}` |

The npm package whitelists the same payload; extraction smokes verify it.
Linux has one thin-binary producer per architecture, shared by packaging,
filesystem image, and guest image. Darwin x64 cross-links on Linux. Darwin
arm64 builds on native `macos-15` with libkrun 1.19.4 revision
`728df8125077d0db44265f6e997c72b81b65c015`, EFI-only features, pinned firmware
and licenses, no GPU or forbidden links, and an ad hoc CI signature.

Release signs dylib before helper under one Developer ID team, grants only the
Hypervisor entitlement, submits one saved payload, and polls that submission.
GitHub and npm publication require `Accepted`; no poll job rebuilds, resigns,
or resubmits.

The Docker FUSE image has `filesystem-base`, `filesystem-dev`, and
`filesystem-release` stages. It contains only the flat thin runtime, without
engine, Wasmtime, provider bundle, or provider-store context. Identity uses
arguments; `OMNIFS_ATTACH_ADDR` is the sole Omnifs launch variable.

PR lanes build and smoke each architecture; `main` merges their digests.
`fuse-docker` tests real-image lifecycle, down ordering, cold start, cross-mount
identity, kill/reattach, and absence of credentials.

### Guest disk image artifact (libkrun runtime)

The libkrun guest is a raw Debian trixie arm64 EFI disk from the `mkosi`
project in `scripts/guest-image/`, with systemd-boot, FUSE, dropbear, and no
cloud-init. `just guest-image` obtains linux/arm64 `omnifs-thin` from the shared
builder or `OMNIFS_THIN_BIN`, then runs `mkosi` in a privileged container. The
image needs no provider store, engine, or Wasmtime.

The default `dev` profile allows root console autologin for smoke and debugging.
`release` leaves Debian's root password locked and enables no autologin.
`check-guest-image.sh` mounts either image read-only in a privileged container.
It checks thin, all six Omnifs units, the enabled seed-mount, filesystem, and
SSH-setup units, and the absence of cloud-init. Release also requires locked
root and no console, tty1, or hvc0 autologin drop-ins.

A per-launch `OMNIFS-SEED` ISO, never cloud-init, carries the exact audited
Filesystem name, runtime instance, attach address, readiness port, and SSH
public key. Guest services invoke flat thin arguments; missing data fails.

Guest boot smoke and conformance are local-only because hosted runners cannot
nest virtualization. Run them for guest boot, seed, or libkrun changes.

Native arm64 CI consumes the shared thin artifact, builds and checks release,
compresses with `zstd -19`, and pushes one
`application/vnd.omnifs.guest-image.v1+zstd` OCI blob to the commit tag. Forks
build and check but do not push. Release retags that single-arch artifact and
attests provenance. `oras` remains CI-only.

Release defaults to `ghcr.io/0xff-ai/omnifs-guest:<version>` and downloads on
first use with `reqwest`, anonymous GHCR auth, accepted current or legacy
manifest media types, and SHA-256 verification before caching. Dev never
downloads and requires `target/guest-image/omnifs-guest.raw`, naming
`just guest-image` when absent.

### Libkrun conformance lane (local-only, never CI)

`just libkrun-conformance` runs the `fuse-libkrun` matrix through Filesystem
shell and proves teardown. It is opt-in, serialized with live mounts, and never
runs or silently passes in hosted CI.

### Documentation checks

`just docs-check` verifies doc links and contract templates, not code symbols or
paths. It is local-only and does not block CI.

## Must not

- Treat missing provider WASM in a fresh worktree as a product regression.
- Use `cargo check --workspace --all-targets` as a host gate.
- Treat host checks as proof of metadata injection; only
  `just build providers` runs the harvester.
- Hand-edit generated schema files as the primary fix.
- Change provider models without regenerating the checked-in schema and its
  focused test.
- Validate only the intended leaf path when parent traversal changed.
- Treat Rust type-checking as enough for `Router::compile` behavior.
- Ignore runtime logs when the mount returns `Input/output error`.
- Treat a local aggregate command as the source of truth when CI runs the lanes directly.
- Keep provider artifacts in the shared target sidecar; do not replace them
  with per-test temporary builds.
- Treat `just docs-check` as code-symbol validation.
- Reintroduce a second copy of the filesystem apt block; edit `filesystem-base` instead.
- Add a fourth `/omnifs` literal instead of updating its three owners together.
- Give the filesystem image an `OMNIFS_HOME` or a provider store. It only ever runs `omnifs-thin --protocol fuse`.
- Push guest images from a contributor machine; only guest-image CI and release
  promotion may push.
- Weaken `check-guest-image.sh`'s release-profile assertions to make a build pass instead of fixing the image.
- Expect libkrun conformance in hosted CI or turn its explicit opt-in skip into
  a silent pass.

## Code

- `just/dev.just`
- `just/npm.just`
- `scripts/ci/build-providers.sh`
- `npm/package.json`
- `scripts/ci/check-doc-links.sh`
- `scripts/ci/check-doc-contracts.sh`
- `crates/omnifs-api/src/control.rs`
- `crates/omnifs-bootstrap/src/lib.rs`
- `crates/omnifs-state/src/lib.rs`
- `crates/omnifs-provider/schema/omnifs.provider.schema.json`
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
- `crates/omnifs-daemon/src/fs_runtime/libkrun.rs`
- `crates/omnifs-libkrun/src`
- `crates/omnifs-daemon/src/fs_runtime/guest_image.rs`
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
tail -n 80 ~/.omnifs-dev/daemon-state/logs/daemon.log
```

Filesystem image, built standalone (no daemon, no attach):

```bash
just filesystem-image
docker run --rm --entrypoint /usr/local/bin/omnifs-thin omnifs-filesystem:dev --version
docker run --rm --entrypoint tail omnifs-filesystem:dev --version | head -1
docker run --rm omnifs-filesystem:dev # fails loudly: OMNIFS_ATTACH_ADDR is unset
```

Guest image, both `mkosi` profiles, and local-only libkrun boot smoke
(`guest-image-smoke` and conformance build the private runtime through
`just libkrun-runtime`):

```bash
just guest-image
scripts/ci/check-guest-image.sh target/guest-image/omnifs-guest.raw dev
GUEST_IMAGE_PROFILE=release OUT_DIR=target/guest-image-release scripts/guest-image/build.sh
scripts/ci/check-guest-image.sh target/guest-image-release/omnifs-guest.raw release
just guest-image-smoke
```
