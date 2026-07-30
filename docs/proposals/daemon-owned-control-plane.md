# Daemon-owned control plane

Status: implemented in `aaad33b00` and subsequent commits.

This record keeps the rationale for the control-plane cutover. It is not an active implementation plan. The current contract is [`docs/contracts/50-control-plane.md`](../contracts/50-control-plane.md); this page records why ownership is split and which retired shapes must not return.

## Settled ownership

The active profile root is resolved by `omnifs-bootstrap::Bootstrap<Client>` and `Bootstrap<Daemon>` from `OMNIFS_HOME` or `$HOME/.omnifs`. The CLI owns client concerns: user-facing commands, OAuth and static-token UX, client config, `ClientOwnerId` and mutation sequencing, filesystem specs and runners, metrics, and daemon spawn. The daemon owns provider artifacts, credentials, mounts, SQLite state and cache, attach listeners, live attachments, and raw log bytes.

The CLI and daemon communicate only through the typed local control protocol in `omnifs-api`: tonic/protobuf gRPC in package `omnifs.control.v1` on the profile's Unix socket. The checked-in schema is compiled with vendored `protoc` at build time; generated Rust is build output, not a checked-in artifact. Unary messages and stream items are bounded to 1 MiB, and log tails to 10,000 lines. Client deadlines are 5 seconds for ordinary finite calls, 30 seconds for serving mutations, and 15 seconds for shutdown around its 10-second filesystem drain. Credential material may cross only in request-side messages on this local socket; it never crosses filesystem attach/TCP or appears in replies, status, inventory, logs, Debug, or Inspector output. The CLI does not inspect daemon SQLite tables, and the daemon does not read client files.

## Why the split exists

Mount and credential state must have one owner. Keeping them in daemon SQLite state lets the serving generation validate provider artifacts, grants, credentials, and mount versions as one transaction. Typed RPC keeps retries explicit through `ClientOwnerId`, mutation scope, sequence, and mutation ID. Filesystem configuration and launch state remain client-owned because the CLI selects a runtime, starts a process or guest, and proves teardown; the daemon sees only live wire attachments.

The profile contains two private trees:

```text
<profile>/
  client/
    owner-id
    mutations.json
    filesystems/specs/
    filesystems/state/
    filesystems/runtime/
    cache/
  daemon-state/
    control-store/state.sqlite3
    cache/
    staging/
    logs/daemon.log
  control.sock
  process.json
  spawn.lock
```

The daemon owns the provider, credential, mount, projection, Wasmtime, clone, and log subtrees below `daemon-state`. The CLI owns filesystem specs, runner state, client metrics, and client configuration below `client` and the profile root `config.toml`.

## Current command surface

The public command grammar is `status`, `down`, `logs`, `inspect`, `doctor`, `setup`, `skill`, `completions`, `version`, `mount add|ls|show|update|reauth|revoke|rm`, `credential ls|rm`, and `fs create|attach|detach|restart|rm|shell|ls --name <id>`. Credential listing exposes only non-secret status. Credential removal deletes local material after consent and does not revoke upstream access. The hidden `daemon` and `run-fs` subcommands are process entry points.

There is no `omnifs up`, `omnifs apply`, offline serving mode, desired/applied Git ref, mount snapshot handoff, or reconcile command. `setup` composes configuration, daemon start, filesystem creation, and attachment. `down` asks the daemon to stop attached filesystems, waits for the bounded drain, and then stops the daemon.

## Security decisions

The control socket is a fixed Unix socket in the profile, protected by a `0700` profile and `0600` socket. The VFS namespace uses a separate fixed Unix socket and one loopback or Docker-bridge TCP listener. TCP attach has no auth and never binds all interfaces; the filesystem runner receives no credentials, provider artifacts, or host mounts.

Doctor owns stray cleanup. It may remove stale process identity, filesystem, or container state only after the daemon is stopped, the user consents, and a fresh exact identity probe succeeds. Logs preserve raw bytes and remain daemon-owned.

## Retired shapes

Do not reintroduce:

- `omnifs-workspace` or a shared workspace broker API;
- `$OMNIFS_HOME/mounts`, Git desired/applied refs, `cache/mount-revisions`, immutable snapshot argv, or offline serving;
- `daemon.json` as a workspace map, JSON credential files, or client reads of daemon state;
- a second apply or reconcile command, remote control endpoint, or TCP authentication mode;
- a daemon container, provider or credential state in filesystem images, or implicit filesystem teardown during daemon replacement.

These deletions are intentional. They remove duplicate ownership and make the RPC boundary enforceable in code and tests.
