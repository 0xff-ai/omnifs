# Inspector emission architecture

Status: implemented (phase 3); TCP loopback transport instead of UDS
Scope: host-side `InspectorSink` emission path, transport from daemon to subscribers, history/replay semantics, recording, `omnifs dev` integration

> **Architecture note — TCP, not UDS.** This document originally
> proposed Unix-domain-socket transport bind-mounted from the
> container to the host. That doesn't work across the Docker Desktop
> VM boundary on macOS and Windows: the bind mount forwards the file
> entry through virtio-fs, but the socket is bound inside the VM's
> network namespace, so host `connect()` returns `ECONNREFUSED`.
> Native Linux docker would work; Docker Desktop does not.
>
> The implemented transport is TCP loopback. The daemon binds
> `0.0.0.0:7878` inside the container; Docker forwards
> `127.0.0.1:7878:7878/tcp` to the host; CLI connects via
> `TcpStream::connect("127.0.0.1:7878")`. All other architecture
> below (ring, broadcast, subscriber lifecycle, redaction, schema)
> applies unchanged.

## Context

Today the inspector stream is a single append-only JSONL file at
`/tmp/omnifs.inspector.jsonl` inside the container, and the CLI consumes it
through `docker exec tail -f`. This works as a debug path but has four
real problems:

1. `InspectorSink` emits under a `Mutex<InspectorLineWriter>`. The hot path (FUSE
   threads, callout future tasks, cache lookups) takes a lock per event.
   At full FUSE traffic this is the only contention point in the runtime.
2. File grows unbounded. A long-running session leaks `/tmp` disk.
3. `docker exec tail -f` is one consumer, started per CLI invocation. No
   way to attach multiple subscribers without spawning multiple tails. No
   way for a subscriber to see what happened *just before* it attached.
4. CLI startup pays a docker round-trip and inherits whatever buffering
   `tail -f` decides on. Latency from emit to TUI render is dominated by
   the bridge, not the work.

The goal is to keep the typed `InspectorEvent` enum and its redaction
discipline exactly as they are, and replace the transport with something
that supports multiple subscribers, bounded memory, lossless emit on the
hot path, and a small history window so a new attach sees recent events.

`InspectorEvent` is not changing. Wire format on the socket is the same
newline-framed JSON the CLI already parses through `omnifs-inspector::parse_record_line`.

## Non-goals

- Replacing the `omnifs-inspector` schema or wire format.
- Migrating to `tracing::instrument` / `tracing-subscriber` / OpenTelemetry.
  Both have been evaluated; both lose the typed-enum guarantees and add
  dependency weight without buying transport features we need.
- Cross-host observability backends. Subscribers are local processes
  (CLI or a future Rust observer).
- Persistent durable storage. The file sink is opt-in for ad-hoc
  recording; it is not a replayable journal.

## Architecture

Three components inside the host daemon:

```
┌──────────────────────────────────────────────────────────────────┐
│ host runtime (FUSE threads, provider futures, callouts, caches)  │
│                                                                  │
│  emit(InspectorEvent) ──┐                                             │
└────────────────────┼─────────────────────────────────────────────┘
                     │ Arc<InspectorRecord>, fan-out:
                     │   - try_push into history ring  (lock-free)
                     │   - broadcast.send()            (non-blocking)
                     │
                     ▼
┌──────────────────────────────────────────────────────────────────┐
│ InspectorSink                                                         │
│   history: crossbeam_queue::ArrayQueue<Arc<InspectorRecord>>          │
│   live:    tokio::sync::broadcast::Sender<Arc<InspectorRecord>>       │
│   record:  Option<Mutex<InspectorLineWriter>>  // opt-in file tee     │
└────────────────────┬─────────────────────────────────────────────┘
                     │
                     ▼
┌──────────────────────────────────────────────────────────────────┐
│ UnixListener task                                                │
│   for each accept:                                               │
│     - snapshot history into per-client BufWriter                 │
│     - subscribe to broadcast                                     │
│     - forward records until disconnect or Lagged                 │
└──────────────────────────────────────────────────────────────────┘
```

The hot path never awaits, never locks for more than O(few-ns), never
allocates beyond the `Arc<InspectorRecord>` it already constructed. The
dispatcher and subscriber tasks live on the tokio runtime.

## Non-blocking publish contract

`InspectorSink::emit(event)` is the only entry point. Constraints:

- Constructs an `Arc<InspectorRecord>` exactly once. `Arc` clone is two atomic
  RMWs.
- Calls `history.push(record.clone())`. `crossbeam_queue::ArrayQueue` is
  wait-free MPMC; `push` returns `Err(record)` when full. On full: `pop`
  the oldest and retry. The oldest is dropped (this is the only place
  history loss can happen).
- Calls `broadcast.send(record)`. Tokio broadcast `send` is non-blocking
  even when receivers are slow — it advances the writer cursor and slow
  receivers will get `RecvError::Lagged(n)` on their next `recv`. Returns
  `Ok(receiver_count)` or `Err(SendError)` if there are no receivers
  (ignored; not an error).
- Optional file tee: `if let Some(record_writer) = &self.record { ... }`.
  When configured, takes a short `Mutex` to append a single line. This is
  the only blocking path; it is opt-in and not on production runs.

No event is ever dropped on the broadcast side. Slow subscribers get
`Lagged` and resume from the latest; their dropped-count is surfaced in
the CLI header. History drops are counted in a `dropped_history` counter
that increments before any subscriber sees the new event, so a fresh
subscriber's history-replay accurately reflects what was retained.

Producers from any thread (FUSE, tokio, blocking thread pool) call
`emit()`. The function takes `&self` and is `Send + Sync`.

## Ring buffer (history)

- Backing type: `crossbeam_queue::ArrayQueue<Arc<InspectorRecord>>`.
- Capacity: **1024 records** (~256 KB at typical record size; tunable
  via `OMNIFS_LIVE_HISTORY_CAP` env var for tests).
- Policy: when full, `emit()` pops the oldest and pushes the new record.
  The popped record is discarded; `dropped_history` counter is
  incremented atomically.
- On subscriber connect: iterate snapshot via `len()` + repeated `pop()`
  is destructive — instead, expose a `snapshot() -> Vec<Arc<InspectorRecord>>`
  that drains a clone. Implementation detail: hold a brief read lock or
  use a parallel `ArcSwap<Vec<Arc<InspectorRecord>>>` that the publisher
  swaps. Either is fine; benchmark before committing.

The ring is in-memory only. It does not survive daemon restart. That
matches today's behavior — a new CLI attach to a fresh daemon sees
nothing until activity resumes.

## Broadcast channel (live)

- Type: `tokio::sync::broadcast::Sender<Arc<InspectorRecord>>`.
- Capacity: **256 records per subscriber** (tokio broadcast is per-slot,
  not per-subscriber; this is the lag tolerance window).
- A subscriber falling more than 256 events behind gets `Lagged(skipped)`
  on its next `recv`, increments its drop counter, and resumes from the
  next available record. No reconnect required.

`Arc<InspectorRecord>` is the broadcast item so cloning is cheap regardless of
how many subscribers are attached.

## Subscriber lifecycle

```
1. UnixListener::accept() → UnixStream
2. Spawn subscriber task.
3. snapshot = live_sink.history_snapshot();
4. for record in snapshot: write JSON line to stream.
5. recv = live_sink.broadcast.subscribe();   // future events only
6. loop:
     match recv.recv().await:
       Ok(record)            → write JSON line; on write error, exit.
       Err(Lagged(n))        → dropped_count += n; write a comment line
                               `# dropped {n} events` so the CLI can show it.
       Err(Closed)           → exit (daemon shutting down).
7. Stream closes on drop.
```

History snapshot happens *before* subscribing to the broadcast. This
means a snapshot record might also arrive via the broadcast (a small
duplicate window). De-dup by `InspectorRecord` identity (`mono_us` + a
running per-process sequence number, or by carrying an emission seq in
the record). Recommended: add a `seq: u64` field to the wire envelope
that the daemon increments on every emit. Subscribers track the highest
seq they've seen and skip lower seqs from the broadcast.

(Schema addition: `seq` is a minor non-breaking field. CLIs that don't
know about it ignore it. The CLI's de-dup is a `> last_seq` check.)

## Transport details

### Listen address

Inside the container: `0.0.0.0:7878` so Docker's port forwarding can
reach the daemon. The daemon refuses to start the server if the bind
fails (port already in use, container running with `--network host`
without authority, etc.).

On the host: `127.0.0.1:7878`, reached via Docker's port mapping.

`omnifs dev` configures `HostConfig.port_bindings` to forward
`container:7878/tcp → 127.0.0.1:7878` on the host. The host-side
binding is restricted to loopback so subscribers can only come from
the local machine.

### Permissions

The transport is plain TCP loopback. There is no per-subscriber
authentication; any process on the host (or inside the container)
that can `connect()` to the port is a subscriber. Acceptable for the
contributor flow on a single-user dev machine; production
(`omnifs up`) revisits this when the security design lands. The
records on the wire are already redacted by the InspectorEvent
constructors, so a snooping subscriber sees only what the design
considers safe to display.

### Listener lifecycle

- Daemon binds the listener on startup. Bind failure is logged and
  the live server simply doesn't run; emit still works locally (ring +
  optional file tee).
- TCP listener has no file artifact; nothing to clean up on shutdown.
  Tokio aborts the accept loop when the runtime drops.

## Configuration

Daemon-side environment variables (in addition to existing `OMNIFS_LIVE`,
`OMNIFS_LIVE_PATH`):

| Var | Default | Meaning |
|---|---|---|
| `OMNIFS_LIVE_ADDR` | `0.0.0.0:7878` | TCP listen address. Set to `""` to disable. |
| `OMNIFS_LIVE_HISTORY_CAP` | `1024` | Ring capacity. |
| `OMNIFS_LIVE_BROADCAST_CAP` | `256` | Per-subscriber lag tolerance. |
| `OMNIFS_LIVE_PATH` | unset | File tee path. **Default is no file.** Set to a path to enable recording. |
| `OMNIFS_LIVE` | `1` | Master disable (sets all sinks off). |

`omnifs dev` sets `OMNIFS_LIVE_ADDR=0.0.0.0:7878` and forwards the
port via Docker. `OMNIFS_LIVE_PATH` is no longer set automatically;
users opt in via `omnifs dev --record <path>` (future flag) or set
the env var directly on the running daemon.

CLI-side: `omnifs inspect` source selection:

| Mode | Source |
|---|---|
| `omnifs inspect` (no flags) | `Socket { addr: 127.0.0.1:7878 }` (TCP) |
| `omnifs inspect --replay <file>` | `Replay(<file>)` |
| `omnifs inspect --record <file>` | Inspector socket source + tee to file on the host side |

The legacy `DockerTail` source is removed. `--container` flag still
exists but only affects the host-side socket path resolution (which
session's socket to attach to).

## Schema additions

Add to `InspectorRecord` envelope:

```rust
pub struct InspectorRecord {
    pub v: u8,
    pub ts: String,
    pub mono_us: u64,
    pub seq: u64,      // NEW: monotonic per-daemon-process sequence
    pub event: InspectorEvent,
}
```

`seq` is daemon-local, increments on every emit (including dropped-history
emits — it represents the emission ordinal, not the retention ordinal).
Subscribers use it for history/broadcast de-dup as described above.

Comment lines (non-JSON, leading `#`) become a valid stream item from
daemon to subscriber for control messages like `# dropped {n} events`.
The CLI parser ignores lines starting with `#`. The on-disk file tee
omits comment lines (it's pure JSONL).

## Implementation phases

### Phase 1: ring buffer + non-blocking publish, file sink stays

- Add `crossbeam-queue` dep.
- Replace `Mutex<InspectorLineWriter>` with the ring + optional file sink.
- Hot path becomes non-blocking; file writes happen on a dedicated
  drainer task that consumes from the ring (or stays inline as today
  for the opt-in case — TBD during implementation; ring is the source
  of truth either way).
- Add `seq` field to envelope. Wire-format-safe (downstream ignores
  unknown fields).
- CLI: no changes. Continues reading the file via docker tail.

Acceptance: existing tests pass; `omnifs inspect` works as before;
benchmark shows hot-path emit drops by at least 1 order of magnitude.

### Phase 2: UnixListener server + broadcast

- Add the broadcast channel.
- Spawn the `UnixListener` task on daemon startup.
- Each subscriber task: history snapshot → broadcast subscribe → forward.
- `omnifs dev` adds the socket bind mount.
- CLI gains `SourceKind::Socket(path)`. Not yet the default.

Acceptance: `omnifs inspect --source socket://...` (provisional flag)
attaches via UDS; multiple parallel CLIs work; one slow CLI doesn't
affect others.

### Phase 3: switch CLI default + retire docker-tail

- `omnifs inspect` defaults to socket.
- Resolve socket path via container session lookup (same logic as
  resolving the container name).
- Remove `DockerTail` source and its `docker exec` machinery.
- File path defaults to off; `--record <file>` enables recording on the
  host side (CLI writes to disk; daemon doesn't).

Acceptance: docs updated, smoke tests against `omnifs dev` confirm
end-to-end. Drop counter visible in CLI header.

## Open questions

- **`omnifs up`** (non-dev) needs an answer for socket placement too.
  Probably reuses the session root pattern; deferred to phase 2.
- **Subscriber authentication.** Production may need to restrict who can
  attach. Probably handled by filesystem permissions on the socket
  directory, but worth confirming once `omnifs up` permissions design
  lands.
- **Ordering across the snapshot/broadcast handoff.** The `seq` field
  resolves de-dup; whether the snapshot order is preserved when
  multiple threads emit concurrently depends on the ring implementation.
  Phase 1 should establish whether `crossbeam_queue::ArrayQueue` gives
  total order or just FIFO-per-producer. If only the latter, we either
  serialize the `Arc<InspectorRecord>` construction (a per-process atomic
  counter for `seq` already does this) or document that snapshot order
  is "approximately emission order, subscribers should sort by `seq`."
