# NFSv4 loopback frontend

Status: accepted
Scope: `crates/omnifs-nfs`, daemon frontend dispatch, host/frontend cache policy

## Context

`omnifs` needs a host-facing filesystem surface for environments where FUSE is
not the right operating-system integration point. macOS is the motivating case:
the host CLI can run locally, while the Linux FUSE runtime remains tied to the
container workflow.

The frontend must preserve the provider model. Providers remain WASM components
driven through the host runtime and shared cache; the operating-system frontend
only adapts kernel filesystem requests into host lookup, listing, read, open,
and invalidation operations.

NFS is a protocol boundary, not a new provider protocol. NFS filehandles,
stateids, leases, client ids, mount options, and RPC encoding are frontend
implementation details. Shared host code continues to speak in namespace paths,
provider paths, objects, attrs, listings, file metadata, and cache records.

## Decision

`omnifs` provides a read-only NFSv4.0 loopback frontend. The frontend is scoped
to:

- ONC RPC over TCP record marking.
- NFS program `100003`, version `4`, procedures `NULL` and `COMPOUND`.
- NFSv4.0 with `minorversion == 0`.
- Read-only namespace projection over the provider runtime and host caches.
- Loopback-only server binding.

The daemon starts this frontend through `omnifsd --frontend nfs`. Daemon
dispatch loads the provider registry, starts provider timers, constructs one
`omnifs_nfs::Export`, starts one loopback NFS server, writes mount state, mounts
the local client, blocks until the mount exits, then shuts providers down.

`Export` is the NFS-local object view. It owns NFS object ids, filehandles,
stateids, per-open snapshots, ranged provider handles, path invalidation, and
NFS-specific eviction. One `Export` is instantiated per NFS mount process; all
NFS connection worker threads share that one instance.

## Namespace

The protocol root exposes the omnifs namespace directly. The client mount source
is `127.0.0.1:/omnifs` so operating systems can label the mounted volume
`omnifs`, but `omnifs` is not synthesized as a listed directory in the exported
tree. Opening the mounted volume lists configured provider mounts immediately,
for example `arxiv`, `db`, `dns`, `docker`, and `github`.

`LOOKUP("omnifs")` against the protocol root resolves only as the hidden export
path needed by the mount handshake. It is not returned by `READDIR`. A provider
mount literally named `omnifs` wins over the hidden export lookup.

Component names are parsed through `ComponentName`. Empty names, `.`, `..`,
slash, backslash, NUL, and non-normal path components are rejected.

## Protocol

The server accepts `AUTH_SYS` and `AUTH_NONE` RPC credentials. Unknown
credential flavors return a denied RPC `AUTH_ERROR` reply with `AUTH_BADCRED`.
Non-`CALL` messages, truncated call headers, truncated auth fields, and malformed
`COMPOUND` payloads return accepted RPC `GARBAGE_ARGS` replies. Unknown
programs, program versions, and procedures return the corresponding ONC RPC
accept status instead of a successful empty body.

The compound evaluator keeps current and saved filehandles, stops on the first
non-OK status, and returns one result body per executed operation. Result
encoders emit raw wire statuses only at the protocol boundary; internal status
plumbing uses `Status`.

| Operation | Behavior |
|---|---|
| `PUTROOTFH`, `PUTPUBFH` | Set the current filehandle to the exported root. |
| `PUTFH` | Decode a process-generation filehandle and verify the object still exists. Stale generation or unknown objects fail. |
| `GETFH` | Return the current filehandle. Filehandles are volatile across server restarts. |
| `GETATTR` | Return supported attrs only. Unsupported requested attrs are omitted. |
| `LOOKUP` | Resolve one validated component below the current directory. |
| `LOOKUPP` | Move to the parent object. |
| `ACCESS` | Report read, lookup, and execute access according to `NodeKind`; write-shaped access bits are not granted. |
| `READDIR` | Return deterministic known entries sorted by `(name, id)`, cookies starting at `3`, and a verifier derived from the directory snapshot. Non-exhaustive provider listings are returned as finite snapshots; explicit `LOOKUP` resolves named dynamic children. |
| `READ` | Validate the stateid, reject directories and symlinks, and read from the open snapshot or ranged provider handle. |
| `READLINK` | Return symlink bytes for symlink objects. Non-symlinks fail with `NFS4ERR_INVAL`. |
| `SAVEFH`, `RESTOREFH` | Save and restore the current filehandle. |
| `SECINFO` | Return the supported `AUTH_SYS` and `AUTH_NONE` flavors. |
| `SETCLIENTID`, `SETCLIENTID_CONFIRM` | Use a process-generation-derived client id and fixed verifier for the loopback subset. |
| `RENEW` | Refresh open state for the matching client id. |
| `OPEN` | Accept read opens with no delegation. Create or write access returns `NFS4ERR_ROFS`; non-zero share deny returns `NFS4ERR_NOTSUPP`. |
| `OPEN_CONFIRM` | Reject valid-but-unexpected confirmations with `NFS4ERR_NOTSUPP`; unknown stateids return `NFS4ERR_BAD_STATEID`. |
| `CLOSE` | Validate and remove the open state, then return the next stateid. |
| `COMMIT`, `CREATE`, `LINK`, `REMOVE`, `RENAME`, `SETATTR`, `WRITE` | Return `NFS4ERR_ROFS`. |
| `LOCK`, `LOCKT`, `LOCKU`, `RELEASE_LOCKOWNER` | Return `NFS4ERR_LOCK_NOTSUPP`. |
| Other operations | Return `NFS4ERR_NOTSUPP`, or `NFS4ERR_OP_ILLEGAL` for explicit illegal op handling. |

The frontend does not support delegations, callback-dependent correctness,
mutation semantics, or directly writable projected files.

## Filehandles, stateids, and leases

Filehandles contain the process generation and object id. They are advertised
as volatile with `FH4_VOLATILE_ANY`, because the server has no durable
filehandle table across process restarts.

`OPEN` creates server-owned stateids through `OpenTable`. A stateid carries the
full process generation and an open id, plus a seqid. `READ`, `CLOSE`, and
`RENEW` validate stateids before reaching provider reads. Unknown stateids
return `NFS4ERR_BAD_STATEID`; old seqids return `NFS4ERR_OLD_STATEID`; expired
state returns `NFS4ERR_EXPIRED`.

The advertised lease is process-lifetime compatibility metadata backed by
tracked `renewed_at` timestamps. Reads and renewals refresh matching open
state. This is not a distributed lock manager and does not provide callback
recall semantics.

## Attributes

Projected files use the shared file-attribute model in
`docs/design/file-attributes.md`. NFS preserves the same high-level truth:
metadata is truthful or explicitly unavailable, and bytes are consistent for
the declared stability scope.

NFS-specific attr rules:

- `FATTR4_FH_EXPIRE_TYPE` is `FH4_VOLATILE_ANY`.
- `FATTR4_TIME_DELTA` is one second because emitted mtimes are whole-second.
- `FATTR4_RDATTR_ERROR` is not advertised for normal `GETATTR`.
- `FATTR4_MOUNTED_ON_FILEID` is not advertised because passthrough trees can
  cross backing filesystem roots.
- Directory `NUMLINKS` is the compatibility value `2`; non-directories report
  `1`.
- `MODE` comes from `NodeKind::mode`.
- `change` is derived from provider version evidence, size, kind, backing
  metadata, and object identity where available. Same-size rewrites do not look
  unchanged when the provider supplies version evidence.
- Unsupported backing types such as sockets, devices, and FIFOs are rejected
  rather than treated as regular files.

For `Size::NonZero` and `Size::Unknown` full-deferred files, NFS materializes
on open, publishes the learned exact size before post-open attrs, and serves
the first read from open state. For ranged files, NFS opens a provider ranged
handle and reads chunks through `read-chunk`; EOF can promote a learned size
when the file is non-volatile and the provider proves the observation complete.

Cached non-exact dirents do not downgrade a learned exact size for the same
stability, byte mode, and version identity.

## Cache and invalidation

The NFS frontend uses the shared provider cache and NFS-local object state. It
does not add a FUSE-like browse cache. Under `noac`, repeated client lookups can
still hit the server, but the warmed server path is a local cache read plus
cache-record decode.

Provider invalidations are drained before attr, provider-backed lookup,
readdir, read, open, and stateid read paths. Matching invalidation paths and
prefixes evict:

- NFS path-to-object entries.
- Object records.
- Expected negative-probe cache entries.
- Full open-state snapshots.
- Ranged provider handles.

Entry eviction is local to the NFS object table. Objects track `last_seen`;
idle leaf entries can be swept after allocation activity, while objects with
active open state are preserved.

## Mount lifecycle

The server binds loopback and rejects non-loopback addresses. The accept loop
is blocking, wakes on shutdown, and caps active worker connections. Accepted
streams have read and write timeouts so idle peers do not hold workers
forever. Dropping `RunningNfsServer` signals shutdown and joins remaining
workers.

The NFS crate owns NFS server and mount mechanics only. The daemon owns provider
registry lifecycle and global provider shutdown. Mount state is written as
structured JSON under a private state directory and removed when the mount
exits. Daemon status detects the active frontend through the mount table rather
than reverse-engineering process command lines.

## Mount options

Mount options are conservative and part of the frontend contract. The code
records each option as a `MountOption` with a rationale, and tests lock the
rendered Linux and macOS option strings.

Linux options:

| Option | Rationale |
|---|---|
| `vers=4.0` | Use the implemented NFSv4.0 subset. |
| `proto=tcp` | Match the loopback TCP listener. |
| `port=<port>` | Use the local random server port. |
| `ro` | Preserve the read-only provider contract. |
| `soft` | Bound recovery from a local server failure. |
| `timeo=5`, `retrans=1` | Avoid long retry tails on local failures. |
| `lookupcache=none` | Keep provider lookups authoritative at the server boundary. |
| `actimeo=0` | Keep provider attrs authoritative at the server boundary. |

macOS options:

| Option | Rationale |
|---|---|
| `vers=4` | Use the macOS-supported NFSv4.0 client path. |
| `tcp` | Match the loopback TCP listener. |
| `port=<port>` | Use the local random server port. |
| `sec=sys` | Use local `AUTH_SYS` credentials only. |
| `ro` | Preserve the read-only provider contract. |
| `intr` | Allow interrupted client waits. |
| `nocallback` | Disable delegation and callback traffic. |
| `noac` | Keep provider attrs authoritative at the server boundary. |
| `nonegnamecache` | Keep provider negative lookups authoritative at the server boundary. |
| `retrycnt=0`, `timeo=5`, `retrans=1` | Avoid long retry tails on local failures. |

macOS NFSv4 mounts do not use `soft` or `locallocks`; macOS rejects that option
set before contacting the server.

## Security

The security model is loopback-only and single-user-oriented:

- The server binds `127.0.0.1`.
- RPC credentials are accepted only as `AUTH_SYS` or `AUTH_NONE`.
- `AUTH_SYS` uid, gid, and groups are advisory for this frontend. They do not
  provide FUSE-equivalent same-host user isolation.
- Mount state lives under a user-private directory with `0700` directory mode
  and private state-file permissions.
- Mount options do not carry tokens, passwords, private keys, or other secrets
  in argv.
- GitHub API auth comes from the provider runtime secret path. Git clone
  passthrough uses the forwarded SSH agent when available; host private keys
  are not copied into runtime state.

The loopback NFS frontend does not claim FUSE-equivalent permission isolation
and is not exposed on non-loopback interfaces.

## Consequences

NFS gives macOS and other non-FUSE clients a normal read-only filesystem mount
without changing the provider protocol. Standard shell tools interact with the
same provider paths, cache records, file attrs, and object identities as the
FUSE surface.

The frontend pays for that compatibility with NFS-specific state: filehandles,
stateids, client ids, open snapshots, mount options, and RPC encoding are all
owned locally by `crates/omnifs-nfs`. Those details must not leak into provider
SDKs, WIT names, host cache semantics, or provider authoring APIs.

Read-only behavior is a hard contract. NFS operations that imply mutation,
locking, delegation, or callback ownership are rejected rather than partially
implemented.

Because filehandles are volatile, open state is process-local, and the server
binds loopback, this frontend is suitable for local projected browsing and
tooling, not as a shared network filesystem service.
