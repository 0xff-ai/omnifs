# Acceptance test plan: CLI ↔ daemon lifecycle

Black-box acceptance coverage for the straightened lifecycle, driving the real
`omnifs` binary against a hermetic home. Every test is isolated and self-cleaning
so the suite never touches the user's real `~/.omnifs`, `~/omnifs`, or port 7878.

## Isolation strategy

Each test gets its own:

- `OMNIFS_HOME` = a fresh `tempfile::tempdir()` (config, cache, providers,
  credentials, mounts all land under it).
- `OMNIFS_MOUNT_POINT` = `<tmp>/mnt` (the daemon serves here, not `$HOME/omnifs`).
- `OMNIFS_DAEMON_ADDR` = `127.0.0.1:<free port>` (both the spawned native daemon
  and the CLI client read this, after the enabler fix below), so tests run in
  parallel without colliding on 7878.

Fixture: copy `target/wasm32-wasip2/release/test_provider.wasm` and
`omnifs_tool_archive.wasm` into `<OMNIFS_HOME>/providers`, and write the no-auth
mount spec `{"provider":"test_provider.wasm","mount":"test","capabilities":{"domains":["httpbin.org"]}}`
to `<OMNIFS_HOME>/mounts/test.json`. The mount serves `test/hello/message`.

A `Drop` guard on the test fixture force-unmounts the mount point and kills any
surviving daemon, so a panicking or interrupted test still cleans up.

Skip (not fail) only when the platform genuinely cannot mount (FUSE off Linux,
NFS loopback unavailable in the sandbox). A daemon that exits from a CLI parse
error is a real failure, not a skip.

## Enabler fixes (prerequisites)

- **Native daemon honors the control address.** `launch_host_native` and
  `write_native_launch_record` (`crates/omnifs-cli/src/launch.rs`) hardcode
  `127.0.0.1:DEFAULT_PORT`. Parse `crate::inspector::daemon_addr()` into the
  control `SocketAddr` (fall back to the default on parse error) so
  `OMNIFS_DAEMON_ADDR` moves the spawned daemon and the client together. Fixes
  the audit finding and makes the suite hermetic.
- **Un-break `frontend_conformance`.** It spawns `omnifs daemon --frontend ...`
  (removed) and pushes `POST /v1/mounts` (removed), so it silently skips. Rework
  it to write the spec to `<OMNIFS_HOME>/mounts/` before spawning (the daemon
  reconciles on start) and to drop `--frontend` (the daemon picks the platform
  default: FUSE on Linux, NFS elsewhere). It must fail loudly if the daemon exits
  on a bad argument, and skip only when the platform's mount truly cannot start.

## Scenarios

Each drives the real CLI (`CARGO_BIN_EXE_omnifs`) with the isolated env above.

| # | Scenario | Steps | Assert |
|---|----------|-------|--------|
| 1 | Status, nothing running | `status` | exit 0, reports not running, no panic |
| 2 | Down, nothing running | `down` | exit 0, "Nothing to tear down." |
| 3 | Up serves the mount | write fixture, `up` | exit 0, mount active, `test/hello/message` readable, `<OMNIFS_HOME>/launch.json` exists |
| 4 | Status while running | (after 3) `status` | exit 0, shows `launch: host_native`, lists the `test` mount |
| 5 | Up while already running | (after 3) `up` | non-zero exit, message names an already-running daemon |
| 6 | Down is clean | (after 3) `down` | exit 0, mount gone from the table, `launch.json` removed, daemon process exited |
| 7 | Dead-daemon fallback | `up`, then `kill -9` the daemon pid, then `down` | exit 0, stale mount swept using the launch record, `launch.json` removed |
| 8 | Failed mount surfaced | add a spec referencing a missing provider, `up`, `status` | the broken mount appears in the failed set with a reason; `test` still serves; `down` cleans up |

## Out of scope (this pass)

- Docker `dev`/`up`/`down` (needs a Docker daemon and an image build; gate on
  Docker availability if added later).
- Provider-contract auto-migration end to end (needs a controlled contract
  change between two provider builds).
