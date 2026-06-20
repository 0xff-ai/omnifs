# Plan: straighten the CLI ↔ daemon ↔ Docker lifecycle

## Goal

Make the daemon own its lifecycle and **self-describe** it, so the CLI never
guesses what it is managing. Replace four disconnected version checks with one
compatibility model. Close the lifecycle and edge-case holes. Build the
provider-contract versioning that upgrade safety depends on. Maximize usability
and DX throughout: every failure is legible, every stop is graceful, every
command agrees on what is running.

This supersedes the earlier "daemon owns lifecycle, CLI as thin client" plan,
whose stages 1-2 (daemon-side materialize, reconcile keystone, `/v1/reconcile`,
`/v1/shutdown`, daemon-owned frontend selection) have already landed as
uncommitted in-flight work. That work is a draft to reshape, not a contract to
preserve.

## What is already landed (in-flight, uncommitted)

- Daemon-side materialize (`crates/omnifs-host/src/materialize.rs`), reconcile
  keystone with per-mount fingerprint (`ProviderRegistry::reconcile`,
  `crates/omnifs-host/src/registry.rs`).
- Control API: `GET /v1/{ready,version,status,mounts,mounts/{name},events}`,
  `POST /v1/{reconcile,shutdown}` (`crates/omnifs-daemon/src/server.rs`).
- Daemon-owned frontend selection and mount-point resolution
  (`FrontendKind::platform_default`, `resolve_mount_point`,
  `crates/omnifs-daemon/src/app.rs`).
- CLI thinned to spawn/supervise + reconcile + shutdown
  (`launch.rs`, `host_launch.rs`, `client.rs`).

## The model we are moving to

1. **The daemon is the source of truth for what is running.** A live daemon
   reports its launch kind, frontend, mount point, and per-mount health over the
   control API. The CLI reads this; it never infers from `[system].runtime`.
2. **A launch record covers the dead-daemon case.** `up`/`dev` write a small
   record at launch and remove it at clean stop. When no daemon answers, `down`
   reads the record to know what to sweep, instead of recomputing defaults.
3. **One compatibility model.** A single control-API version (major.minor) is
   the spine; the image label and the provider contract hang off it with
   explicit, non-silent rules. No misleading comments, no permissive bypass.
4. **Every stop is graceful and symmetric.** The daemon handles signals the same
   way it handles `POST /v1/shutdown`: unmount, drop providers, clear the launch
   record. Docker and native stop through the same daemon path.
5. **The backend is transparent to the CLI.** Only `setup` (choose and record
   the backend) and `up`/`dev` (supply backend-specific launch info) are
   backend-aware. Every other command (`down`, `status`, `mounts`, `reset`,
   `logs`, `shell`, `inspect`) operates through the control API and the launch
   record and never branches on native-vs-docker. One data model unifies the
   parameters across backends and generates the arguments handed to the daemon.

## Wire and persistence contracts (the irreversible surfaces)

These are the shapes that, once shipped, are expensive to change. Stated
concretely here so they are decided before any code lands.

### Backend abstraction and unified params (CLI data model)

One data model holds the launch parameters, common and backend-specific, and is
the single place that turns intent into daemon arguments. The backend is a
closed enum, not an open plugin point (two backends, no hypothetical third).

```rust
/// Backend-agnostic launch intent plus the chosen backend's specifics.
struct LaunchParams {
    config_dir: PathBuf,
    cache_dir: PathBuf,
    control_addr: SocketAddr,
    mount_point: Option<PathBuf>,   // None lets the daemon resolve its default
    backend: Backend,
}

enum Backend {
    Native,
    Docker { container_name: ContainerName, image: ImageRef, extra_binds: Vec<String> },
}

impl LaunchParams {
    /// The daemon's own arguments, identical in meaning across backends.
    /// Native renders them as argv for the child process; Docker renders them as
    /// the container env the entrypoint reads. One generator, two renderings, so
    /// the two paths cannot set different flags (the `--host-native` /
    /// `--root-symlinks` divergence goes away).
    fn daemon_args(&self) -> DaemonArgv { ... }
}
```

`Backend` owns `launch(&LaunchParams)` (spawn the child or `docker create`+start)
and `stop()` (signal+wait or container stop+remove). `up`/`dev` build a
`LaunchParams` and call `backend.launch`; `down`/`reset` reconstruct the backend
from the launch record and call `backend.stop`; `status`/`mounts`/`logs`/`shell`
go through the control API and never name a backend. The backend-specific
surface is confined to constructing `LaunchParams` in `setup`/`up`/`dev`.

### Launch record (new persistence)

The persisted form of `LaunchParams`: written by the CLI at `up`/`dev` once the
daemon is ready, removed on clean `down`. Lives at `<config_dir>/launch.json`
(inside `OMNIFS_HOME`, so it is visible to both the host CLI and a containerized
daemon through the existing bind). The dead-daemon truth source, and what
`down`/`reset` rebuild the backend from.

```jsonc
{
  "version": 1,
  "runtime": "native",                 // "native" | "docker"
  "control_addr": "127.0.0.1:7878",
  "mount_point": "/Users/you/omnifs",
  "daemon_pid": 12345,                 // native only; null for docker
  "container_name": "omnifs",          // docker only; null for native
  "image": "ghcr.io/0xff-ai/omnifs:0.3.0", // docker only
  "started_at": "2026-06-19T12:00:00Z"
}
```

Atomic write (temp + rename). A record whose `version` this CLI does not
understand is reported, not silently ignored (same discipline as the NFS
`STATE_VERSION` skip counter).

### DaemonStatus additions (wire)

`DaemonStatus` (`crates/omnifs-api/src/lib.rs`) gains two fields so a live
daemon is fully self-describing:

```rust
/// How this daemon was launched. Lets the CLI tear down and report the
/// right runtime without inferring from config.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LaunchKind { HostNative, Container }

// added to DaemonStatus:
pub launch: LaunchKind,
/// Mounts that did not converge at the last reconcile, with reasons.
/// Empty when every desired mount is serving.
pub failed: Vec<MountFailure>,
```

The daemon remembers the last reconcile's `failed` set (a field on `Daemon` or
the registry) so `omnifs status` can show a dark mount and why, instead of it
simply being absent from `mounts`.

### Control API version (wire)

`API_VERSION: u32` becomes a structured version. The gate: **refuse on major
mismatch, warn on minor skew, proceed.** The lying "major version" comment dies
with the thing it described.

```rust
pub const API_MAJOR: u16 = 1;
pub const API_MINOR: u16 = 0;

// VersionInfo carries both:
pub api_major: u16,
pub api_minor: u16,
```

`DaemonClient::probe` accepts `api_major == API_MAJOR` (any minor), and emits a
one-line warning when the minors differ. `api_version: u32` is kept as a
derived `major * 1000 + minor` only if needed for the on-the-wire transition;
otherwise removed. The checked-in `daemon.json` regenerates in the same change.

### Image compatibility (derivation, not a fourth number)

The image's `ai.0xff.omnifs.launch-protocol` label becomes
`daemon-control-v<API_MAJOR>` (derived from the API major, not a free string),
so the image-label check and the control-API check are one fact in two places
that cannot drift. The `min-launcher-version` semver gate stays, but the three
permissive holes (missing, `"unknown"`, unparseable) each log an explicit
"compatibility check skipped because X" line rather than passing quietly.

### Contract block (persistence, Spec) — slice 4

Stamped into each mount spec by `omnifs init`, per
`docs/future/provider-contract-versioning.md`. Exact shape is fixed at the start
of slice 4, after resolving the doc's open question (where the provider config
schema lives). Provisional:

```rust
pub struct Contract {
    pub config_fields: Vec<ContractField>,   // { name: String, required: bool }
    pub capabilities: Vec<ContractCapability>,// { kind: String, value: String }
    pub auth_scheme: Option<String>,
    pub provider_version: String,            // provenance label only
}
```

## Slices

Each slice ends green on `cargo fmt && cargo nextest run` (default members), and
lifecycle/runtime slices additionally validate through the live runtime
(`omnifs dev`, exercise the mount) per `CONTRIBUTING.md`. Slices land in order;
later slices depend on earlier ones.

### Slice 0 — foundations and cleanup (no wire surface)

Outcome: every teardown path, present and future, has a clean daemon-side
shutdown to call; dead code and the proxy flag are gone.

- **Daemon signal handler.** In `crates/omnifs-daemon/src/app.rs`, spawn a
  `tokio::signal` task on the runtime that, on `SIGTERM`/`SIGINT`, calls the
  existing `Daemon::trigger_shutdown` (unmount → `serve` unblocks → providers
  dropped → exit). This makes the future launchd/systemd `stop`, the macOS
  teardown `kill -TERM`, and Docker `stop` all converge on one clean path.
- **Decouple `host_native` from `--root-symlinks`.** Add an explicit
  `--host-native` bool to `DaemonArgs`; `--root-symlinks` reverts to meaning
  only "maintain `/<mount>` symlinks." Native spawn (`host_launch.rs`) passes
  `--host-native`; the container entrypoint passes only `--root-symlinks`.
  Delete the `host_native = !args.root_symlinks` proxy in `app.rs`.
- **Delete `crates/omnifs-cli/src/presentation.rs`** (dead, per `notes.md`) and
  any `mod presentation;` reference.
- **`detach()` drops the child** instead of `std::mem::forget`
  (`host_launch.rs`): give `HostDaemon` an `Option<Child>` and take it, or set
  `kill_on_drop(false)` and let it drop. No leaked pidfd.

Verify: `cargo nextest run`; `omnifs dev` then `docker stop omnifs` leaves no
host-visible mount and the daemon log shows a clean unmount.

### Slice 1 — backend abstraction and runtime detection (the central fix)

Outcome: the backend is transparent everywhere but `setup`/`up`/`dev`; `down`,
`status`, and `reset` act on what is actually running, never on `[system].runtime`.

- **Backend + params data model.** New `crates/omnifs-cli/src/backend.rs`:
  `LaunchParams`, `Backend` (enum above), `Backend::launch`/`stop`, and
  `LaunchParams::daemon_args` as the single argument generator. The existing
  native spawn (`host_launch.rs`) and Docker create (`runtime.rs`) become the
  two `launch` implementations behind it; the duplicated arg/env wiring collapses
  into `daemon_args`.
- **Launch record I/O.** New `crates/omnifs-cli/src/launch_record.rs`: the
  persisted `LaunchParams` (schema above) with `write_atomic`, `read`, `remove`,
  and `into_backend()`. `up`/`dev` write it once the daemon is ready; clean
  `down` removes it.
- **Daemon self-description.** Add `LaunchKind` and `failed` to `DaemonStatus`
  and `version_info`/`status` in `server.rs`; the daemon stores the last
  reconcile's failures. Regenerate `daemon.json`.
- **Backend-transparent teardown.** `down.rs`/`reset.rs` stop branching on
  `config.runtime()` and stop naming Docker or native. Resolution order: probe
  the control port (live daemon → trust `DaemonStatus.launch`); else read the
  launch record; else nothing is running. Either way, dispatch through
  `Backend::stop`: graceful `client.shutdown()` first (the slice-0 signal handler
  makes `docker stop` and native `kill` both clean), then backend-specific
  reclaim (container remove / wait-unmounted), with the dead-daemon sweep keyed
  off the record's `mount_point`.
- **Single mount-point truth.** Delete `paths::default_host_mount_point`; the CLI
  reads `mount_point` from status (live) or the launch record (dead).
- **Move k8s teardown out of `down`.** The dev-cluster teardown
  (`down.rs:50-54`) moves to a `dev`-scoped path; production `down` stops
  touching it.

Verify: `omnifs dev` then `omnifs down` on a **default (native) config** removes
the dev container (the headline bug); switching `[system].runtime` after `up`
still tears down the real runtime; `down`/`status`/`reset` contain no
native-vs-docker branch; `cargo nextest run`; regenerated `daemon.json` matches.

### Slice 2 — edge-case hardening

Outcome: no unmanageable mounts, no silent dark mounts, idempotent `up`.

- **Bind failure is fatal.** `app.rs` returns an error if the control listener
  cannot bind, instead of serving an unreachable mount. The message names the
  likely cause (port in use, another daemon) and points at `omnifs down`.
- **Surface failed mounts.** `omnifs status` renders `DaemonStatus.failed` with
  reasons (`crates/omnifs-cli/src/status.rs`). A dark mount is visible with its
  failure, not absent.
- **Non-destructive `up`.** `runtime::launch_container` skips remove+recreate
  when a running container already matches the desired image and the launch
  record agrees; only recreate on change. Re-running `up` on an unchanged,
  healthy setup is a no-op that reconciles, not a teardown.
- **`reset` stops gracefully.** Route `reset` through the same
  `client.shutdown()`-then-remove path as `down` before deleting configs.

Verify: starting a second daemon on a taken port fails loudly; a mount with a
missing credential shows in `status` with its reason; `omnifs up` twice in a row
does not recreate the container; `cargo nextest run`.

### Slice 3 — unified compatibility model

Outcome: one version spine, no drift between image and API checks, no silent
bypass.

- **Structured API version** (wire shape above) in `omnifs-api`; `probe`
  refuses on major mismatch, warns on minor.
- **Derive the launch-protocol label** from `API_MAJOR`
  (`daemon-control-v{API_MAJOR}`) in `Dockerfile` and the `runtime.rs` check, so
  the two cannot disagree.
- **Close the permissive holes** in `check_launcher_compat`: each skip path logs
  an explicit reason; document the policy in one place.
- **Default image tag resilience.** When the default tag
  (`ghcr.io/0xff-ai/omnifs:<cli-version>`) is absent on the registry, the error
  is actionable (name the tag, suggest `--image` or a channel tag) rather than a
  raw pull 404.

Verify: a daemon one minor ahead warns and proceeds; one major ahead refuses
with a clear message; `cargo nextest run`; regenerated `daemon.json`.

### Slice 4 — provider-contract versioning

Outcome: a spec carries the provider contract it was built against; upgrade
reconciles the two instead of failing cryptically or drifting silently. Built
last, on the stable base, per `docs/future/provider-contract-versioning.md`.

- **Resolve the open question first.** Locate or define the provider config-field
  schema (SDK side, manifest side, or the `omnifs init` path). This sizes the
  additive-config branch. Bring the concrete `Contract` shape back before
  building the rest of the slice.
- **Contract block on `Spec`** (`crates/omnifs-mount`); `omnifs init` stamps it.
- **Contract hash + structural diff**; `omnifs up` pre-flight classifies each
  mount (identical / additive / breaking / capability-or-auth / removed) and
  routes per the doc's table (auto-migrate / prompt / re-consent / hard error).
- **Daemon backstop.** `materialize` refuses a drifted mount with a typed
  `ContractMismatch` that surfaces as a `MountFailure` (now visible in `status`
  via slice 2), guaranteeing the daemon never serves a contract the spec was not
  written against.

Verify: an upgrade that adds an optional config field auto-migrates; one that
changes a capability requires re-consent; one whose provider was removed hard
errors; the daemon refuses a hand-edited drifted spec; `cargo nextest run`.

## Risk and sequencing

- **Riskiest assumption:** that the daemon can cleanly self-unmount on a signal
  while `serve` blocks the main thread. Mitigation: `trigger_shutdown` already
  unmounts from a detached task and unblocks `serve`; the signal handler just
  calls it. Proven in slice 0 before anything depends on it.
- **Second risk:** the contract-versioning open question (slice 4) may collapse
  the additive branch to nothing if config is validated dynamically inside
  providers. Resolved by investigation at the top of slice 4, not assumed.
- **Wire changes are front-loaded** (slices 1 and 3) so the checked-in
  `daemon.json` and the launch-record schema settle early and later slices build
  on stable formats.

## Out of scope

- OS-managed service install (launchd/systemd), socket activation, unix-domain
  control socket. The slice-0 signal handler makes the daemon service-ready;
  installing the service is deferred.
- Changing the WIT provider contract or granting new provider authority.
- Multi-instance addressing beyond making `control_addr` honored consistently;
  one omnifs per machine remains the assumption.
