# Control plane contracts

Status: current-contract
Owns: CLI/daemon ownership, local RPC, profiles, resources, filesystem
runtimes, and contributor state.

## Read when

Read this before changing CLI, API, bootstrap, daemon, state, lifecycle, status,
resource RPC, filesystem runtimes, logs, or `scripts/dev.ts`.

## Rules

### Ownership boundary

There is no shared workspace store. `Profile` resolves `OMNIFS_HOME` or
`$HOME/.omnifs`, owns fixed pre-RPC paths, and exposes `SpawnLock` and exact
`DaemonIdentity` operations.

The CLI owns commands, auth UX, profile config, metrics, daemon spawn, and
resource authoring. Interactive commands edit the complete desired set through
plan/apply and follow progress or durable actions. KCL plan/apply is the
automation path; `credential set --from-env` is the sole secret automation
command. Every write uses typed RPC; the CLI keeps no desired-state reader or
journal.

The daemon owns resources, SQLite, caches, provider preparation, Filesystem
lifecycle, attach endpoints, VFS sessions, and raw logs under
`<profile>/daemon-state/`. It never reads client files or chooses client config.

The only CLI-to-daemon API is tonic/protobuf gRPC over the profile Unix socket,
using checked-in `omnifs.control.v1` and generated Rust. It covers control,
resources, progress, actions, Filesystems, recovery, shutdown, Inspector, and
bounded logs. `ApplyResources` validates and commits one transaction, wakes
reconcilers, and returns before any provider or Filesystem work.

Progress registers before reading its full snapshot. Fanout is bounded and
non-blocking; slow consumers receive a resync snapshot. Revision streams include
only relevant work. Closed event variants carry factual stages, counts, retries,
queues, and outcomes, never inferred percentages or cache hits.

Credential and Filesystem restart actions use client IDs and generation
preconditions. SQLite retains at most one non-terminal action per target across
restart and returns durable receipts. Action dedupe never stores or hashes the
submitted secret bytes; the first ID wins. Secrets cross only in request
payloads on the local control socket and never appear on attach, output, logs,
or progress.

The profile is mode `0700`; `control.sock` and process identity are `0600`.
VFS separately uses `daemon-state/local.sock` and one profile-derived loopback
or Docker-bridge TCP port. TCP has no auth and never binds all interfaces. Both
listeners must bind for readiness and remain alive.

Process identity is diagnostic. Reachable RPC status and inventory are
authoritative. `doctor` uses the live daemon's `RunDoctor` and
`ApplyDoctorRepairs` RPCs. The daemon offers a runtime repair only when desired
and observed Filesystem states are both absent at diagnosis, rechecks that
absence on apply, and reconfirms exact runtime identity before the effect. If
the daemon is unreachable, the CLI may only clean the exact stale bootstrap
identity with its local `CleanStaleInstance` remediation; it never repairs
runtime state.

### Command grammar

One public `omnifs` binary hides `daemon` and `run-fs`. Public commands are:

- `status`, `plan`, `apply`, `down`, `logs`, `inspect`, `doctor`, `setup`,
  `skill`, `completions`, and `version`.
- `provider add|ls|show|rm` imports embedded or local Wasm and pins a Provider;
  import alone grants no authority.
- `mount add|ls|show|update|reauth|revoke|rm` collects typed config, host
  resources, limits, and Credential references, then plans, applies, and
  follows. `reauth` changes material; `revoke` performs upstream revocation and
  leaves `NeedsSecret`.
- `credential login|ls|show|rm|revoke` and
  `credential set <name> --from-env <variable>` keep values out of argv and
  output. Only `set --from-env` automates secrets.
- `fs add|ls|show|rm|restart|shell <name>` owns public filesystem
  lifecycle through resources and actions. Hidden `run-fs` stays internal.

Global output and interaction flags apply after Clap parsing. JSON emits one
terminal envelope; JSONL emits stream records then one terminal result or
error; Clap usage errors exit 2 first.

Human, JSONL, JSON, and quiet mutation commands all wait for terminal
reconciliation by default. Human and JSONL modes stream typed progress. JSON
buffers progress and emits exactly one terminal envelope. Quiet mode emits
only the terminal receipt. Non-TTY human output uses stable lines with no
cursor control. TTY output may show elapsed time for an active stage but never
turns it into a completion percentage or cache-hit claim.

Finite control calls use bounded unary deadlines. Progress and log streams do
not inherit a unary deadline; their target terminal state or Ctrl-C ends them.

#### Errors and recovery

A CLI error states the blocking condition, the final observed state, and one
recovery action that can run under that condition. It must not recommend a
command whose own precondition is the condition that just failed.

Where command grammar permits selector inference, resolve every unambiguous
selector before reporting a missing-selector error. When the user supplies an
explicit protocol, runtime, or platform tuple, validate that tuple before a
broader default-policy guard so the error names the unsupported combination.

Receipts for successful mutations include the next command needed to use or
inspect the result when that step is not automatic. Acceptance tests assert
these visible commands and messages, not only the underlying resource state.

Ordinary RPC calls reject incompatible protocol versions. Teardown must remain
actionable under version skew. If the current CLI can prove that an older
daemon's shutdown request and response are compatible, it may use only that
narrow shutdown operation. Otherwise the mismatch error must name an exact
out-of-band recovery path. It must not tell the user to run `omnifs down` when
the same version check prevents `omnifs down` from sending `Shutdown`.

Ctrl-C stops only the client watch, exits 130, and prints the exact
`status --follow --revision <n>` or `status --follow --action <id>` command
needed to resume. It never cancels daemon work. A plan accepted after an
interactive prompt applies against that exact base revision; stale apply fails
and the CLI does not silently re-plan or ask again.

`setup` starts the daemon, presents embedded providers honestly, creates
Provider and Mount resources plus the recommended Filesystem in one consented
set, and follows its revision. There is no `up` or offline product mode; KCL
plan and apply are the automation surface.

`down` rejects writes, drains exact observed runtimes, reports stragglers, and
stops without deleting desired Filesystems, which the next daemon restores.

### KCL authoring boundary

`omnifs plan <file>` and `omnifs apply <file> --yes` evaluate KCL once through
the official in-process Rust API pinned to one exact upstream revision. They
never invoke a `kcl` subprocess or fetch a remote package implicitly. Imports
must resolve from explicit local input. Local provider paths are client-only:
the CLI reads and validates the artifact, imports it by digest, and sends no
source path to the daemon.

KCL output is temporary JSON interchange decoded immediately into strict Rust
resource types. Rust types and SQLite rows remain authoritative. There is no
KCL schema asset and no generated Rust model derived from KCL. Secrets never
enter KCL. The evaluator runs once per command so consent applies to the exact
candidate that reaches `PlanResources` and `ApplyResources`.

### Mounts, providers, and credentials

SQLite alone owns desired Provider, Mount, Credential, and Filesystem
definitions. They commit as one set with exact base revision and digest.
Durable receipts recover lost replies. Edits use complete-set apply; operations
use durable actions.

The normalized typed resource set owns digest identity. Do not hash KCL text,
presentation JSON, protobuf bytes, or SQLite row layout. A resource change must
update the explicit canonical digest encoding. `ResourceName` has one grammar
in `omnifs-core` shared by every resource kind. Apply policy belongs in the
resource transaction owner, not in SQL row codecs.

`ImportProvider` stores bounded uploads or exact embedded providers in daemon
state after digest and metadata validation. Receipts key on content digest;
identical bytes return `Unchanged`.

Credentials live in daemon SQLite. The CLI owns browser, device, and
static-token UX and submits material beside the planned Credential resource.
The daemon injects only into host callouts and exposes no values, file paths, or
reload command. Status is non-secret; login, set, and revoke follow durable
actions through refresh, drain, and upstream work.

### Filesystems and attach

`FilesystemSpec` owns protocol, runtime, resolved location, and assets. SQLite
resources are desired truth; observed rows retain exact spec, version, runtime
instance, phase, retry, action generation, and tombstone until teardown proof.
Host locations are absolute; guests use `/omnifs`.

`FilesystemSupervisor` owns bounded host, Docker, and libkrun lifecycle through
the daemon's private `fs_runtime` module. It persists identity before effects,
adopts only exact runtime and session matches, serializes each Filesystem, and
retains restart actions across daemon restart. Runners stay out of process and
credential-free.

`RunDoctor` returns findings and opaque remediation IDs. `ApplyDoctorRepairs`
executes only daemon-owned runtime repairs after the eligibility and identity
checks above. Mount reauth remains a client remediation because it needs
interactive credentials.

`VfsServer` owns listeners, readiness, reconnect, pushed stop, and live
sessions. VFS v11 handshakes carry Filesystem name, exact spec, and runtime
instance; only the expected identity is admitted. Desired Filesystems and live
sessions remain separate.

### Logs, Inspector, metrics, and dev

The daemon appends raw log bytes; `omnifs logs` reads them through bounded
`StreamLogs`. Inspector uses its typed stream.

`omnifs-inspector` owns Inspector state, replay and live-event sources, terminal
lifecycle, and TUI rendering. `omnifs-cli` resolves the profile endpoint,
dispatches the command, and renders the final session receipt. Inspector
restores both terminal and prior panic-hook state before returning to the CLI.

CLI dogfood metrics are local files under `<profile>/metrics/`, controlled by
config or `OMNIFS_METRICS`. They are never sent and cannot fail a command.

`scripts/dev.ts` owns a dedicated contributor profile. It builds providers and
CLI, renders KCL, sets credentials through `credential set --from-env`, applies
with `target/debug/omnifs`, waits for the revision, then opens `fs shell
dev-docker` at `/omnifs`. It uses no interactive porcelain or daemon container.

## Must not

- Add a second desired-state or workspace authority beside daemon SQLite.
- Add `up`, a second mutation path, or offline mode. `omnifs apply` is the KCL
  complete-set command.
- Add a KCL schema authority or `kcl` subprocess.
- Send credentials through attach/TCP or expose them in replies, status,
  inventory, logs, tracing, metrics, Debug, Inspector, or receipts.
- Let the daemon read client desired state, normal lifecycle write a client
  filesystem tree, or the CLI read daemon SQLite or logs directly.
- Add remote control or TCP authentication. TCP attach stays unauthenticated on
  loopback or the detected Docker bridge.
- Clear observed Filesystem identity or a deletion tombstone before exact runtime and session teardown is proved.

## Code

- `crates/omnifs-bootstrap/src/lib.rs`
- `crates/omnifs-api/src/control.rs`
- `crates/omnifs-cli/src/rpc.rs`
- `crates/omnifs-kcl/src/lib.rs`
- `crates/omnifs-inspector/src/lib.rs`
- `crates/omnifs-daemon/src/app.rs`
- `crates/omnifs-daemon/src/control.rs` and `crates/omnifs-daemon/src/control/`
- `crates/omnifs-daemon/src/daemon.rs`
- `crates/omnifs-daemon/src/log_stream.rs`
- `crates/omnifs-daemon/src/resource_control.rs`
- `crates/omnifs-daemon/src/progress.rs`
- `crates/omnifs-daemon/src/doctor.rs`
- `crates/omnifs-daemon/src/filesystem_supervisor.rs`
- `crates/omnifs-daemon/src/fs_runtime`
- `crates/omnifs-state/src/lib.rs`
- `crates/omnifs-state/src/resource.rs`
- `crates/omnifs-state/src/action.rs`
- `crates/omnifs-vfs/src/frame.rs`
- `crates/omnifs-vfs/src/server.rs`
- `crates/omnifs-vfs/src/serving.rs`
- `scripts/dev.ts`

## Validation

- Run `just docs-check` for documentation-only changes.
- Control protocol changes need typed request/reply and lifecycle tests across
  API, CLI, daemon, and `omnifs-itest/tests/control_plane`.
- For filesystem behavior, use `just dev -y`, `target/debug/omnifs status`, and the relevant live smoke path.
