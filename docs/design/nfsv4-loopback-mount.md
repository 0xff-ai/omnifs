# Embedded NFSv4.1 loopback as the v0.3 mount surface

Status: proposed
Scope: replace `crates/host/src/fuse/` and `crates/host/src/mount.rs` with a new mount surface; touch `crates/cli` (mount subcommand), `Cargo.toml` (drop `fuser`, add NFS server crates), `Dockerfile` and `compose.yaml` (drop `/dev/fuse` and `SYS_ADMIN`); leave `crates/host/src/runtime/`, `crates/host/src/cache/`, the WIT, the SDK, and every provider unchanged.
Branch: design/nfsv4-loopback

## Context

omnifs today is FUSE-on-Linux. macOS support is the next-largest gap and the chosen path is to skip the macFUSE bridge entirely and replace the FUSE upcall layer with a loopback NFSv4.1 server embedded in the `omnifs` binary. The mount surface becomes:

```
kernel NFS client  ──TCP/127.0.0.1──▶  omnifs nfsd  ──▶  router / inode table / browse cache / runtime
```

This is the same shape FUSE-T uses on Mac, made first-party and open source, and applied uniformly across Mac, Linux, and Windows.

The driving constraints are:

1. **No kernel extension on Mac.** macFUSE requires a kext approval + reboot in Reduced Security mode on Apple Silicon, and Apple is on a multi-year glide path to removing kexts. FUSE-T is closed-source. FSKit needs an app bundle with Developer ID. NFS is the only file protocol macOS ships a native client for that needs none of the above.
2. **No proprietary userland dependency.** FUSE-T's daemon is closed source and personal-use-only. We don't take that on as a runtime dependency for an OSS project.
3. **Performance must be credible.** macFUSE has lower per-op latency in microbenchmarks. NFSv4.1 closes the gap (and beats it on listings) given correct knob settings; see [§Performance](#performance).
4. **Single mount surface across OSes.** The same NFS server serves Mac, Linux, and Windows clients. Cross-platform divergence collapses to client-side mount flags.

## Decisions

### D1. NFSv4.1, not v3 or v4.0

NFSv4.1 (RFC 5661, 2010) is the floor. v3 lacks a notification channel and burns a UDP/TCP port per client; v4.0 lacks sessions and has weaker EXACTLY-ONCE semantics. v4.1 adds:

- **Sessions** with per-slot reply caches → exactly-once RPC semantics, no client-side retransmit storms during slow upstream calls.
- **Trunking** support → multiple parallel client connections multiplex against the same session, which matters under concurrent agents/IDE/shell traffic.
- **CB_NOTIFY / CB_NOTIFY_DEVICEID / CB_RECALL** callbacks on the back-channel → server-pushed cache invalidation, the moral equivalent of `fuser::Notifier::inval_entry`.

We do **not** target v4.2. Its features (server-side copy, sparse files, application data blocks, security labels) are not relevant to a projected, mostly-read-only filesystem. Implementing minor-version 1 and stopping there limits the protocol surface.

### D2. Read-only first; mutations stay out of scope until the existing mutation design lands

The current host serves read-only paths plus bind-mounted clones. The NFS server preserves that. `OPEN` only accepts read modes; `WRITE`, `CREATE`, `LINK`, `RENAME`, `REMOVE`, `SETATTR` (other than no-op size queries) return `NFS4ERR_ROFS`. When the mutation protocol lands per `AGENTS.md` §"Mutation protocol," it will land on top of this server, not in it.

### D3. The router, inode table, browse caches, and callout runtime stay unchanged

The existing host has a clean split between the protocol-facing layer (`crates/host/src/fuse/`) and the routing/cache/runtime layer (`crates/host/src/runtime/`, `crates/host/src/cache/`, the WIT bindings, the SDK). The NFS server replaces the former; everything below it is unchanged.

The boundary the new server consumes is the same one `FuseFs` consumes today:

- `registry.get(mount).call_lookup_child(parent_path, name)` → `LookupResult`
- `registry.get(mount).call_list_children(path)` → `ListResult`
- `registry.get(mount).call_read_file(path, offset, len)` → bytes
- `NodeEntry { mount_name, path, kind, size, backing_path }` from the inode table
- `BrowseCacheL0` and L2 caches keyed by `(mount, path)`
- `cache_delete_path` / `cache_delete_prefix` driven by `event-outcome` from providers

The new file is `crates/host/src/nfsd/mod.rs`, structurally analogous to `crates/host/src/fuse/mod.rs`. The `Filesystem` trait impl becomes a COMPOUND op dispatcher.

### D4. Loopback transport, TCP only

The server binds a TCP socket on `127.0.0.1:<port>`. macOS NFS client does not support Unix domain sockets, and Windows doesn't either; the cross-OS lowest common denominator is loopback TCP. On Linux we still use TCP rather than introducing a second transport — the Linux NFS client is happy with loopback TCP, and using one transport shape keeps the test matrix small.

The port is allocated dynamically (bind to `127.0.0.1:0`, read back). The CLI writes the chosen port into a runtime state file (`$OMNIFS_STATE_DIR/mount.json`) so subsequent `omnifs status` / `omnifs unmount` invocations can locate it. The state file also records the mount point and PID for crash recovery.

### D5. The exported tree is the existing root namespace, unchanged

NFS exports map to the same root the FUSE filesystem builds today: one virtual root containing one entry per configured mount (`/github`, `/dns`, `/arxiv`, …). The NFS `pseudo-root` (FH = `0x00 * NFS4_FHSIZE`) is the synthetic root; per-mount roots are file handles with a stable `mount_id` prefix.

### D6. File handles encode `(mount_id, inode)`, persistently

NFSv4 file handles are opaque to the client and can be up to 128 bytes. We encode:

```
[u8; 2]  version       = 0x01 0x00
[u8; 6]  mount_id      = first 6 bytes of blake3(mount_name)
[u8; 8]  inode         = host inode number, big-endian
[u8; 8]  generation    = host inode generation, big-endian (0 for stable inodes)
```

24 bytes, well under the 128-byte limit. The mount-id prefix lets us route the FH to the right mount without scanning. Generation is reserved for the post-`v0.3` "persistent inodes across remounts" work in the README's roadmap; until then it's zero and the FH is valid only for the current process lifetime. Clients see `NFS4ERR_STALE` after remount, which is correct.

### D7. Bind-mounted clones served by passthrough, not NFS referral

Today, `_repo` paths live on `NodeEntry.backing_path` and FUSE reads pass through to the underlying directory. NFS supports `fs_locations` referrals (mount a different server here), but Mac's referral support is rough and we don't want a second NFS endpoint per clone. The NFS server reads through `backing_path` directly: `READ` issues a `pread` against the backing file, `READDIR` enumerates the backing directory and synthesizes entries. The FH for a backing-path inode is the same shape as any other; only the read implementation differs. macOS doesn't have null-mounts, but it doesn't need them — passthrough is a server-side concern.

### D8. Invalidation rides NFSv4 callbacks, not client TTL expiry

The mount is configured with `actimeo=86400` (one day) and `nordirplus=0` to make the client trust its caches aggressively. The server is the source of truth for cache invalidation, and pushes via the back-channel:

| Provider event-outcome | NFS callback |
| --- | --- |
| `invalidate-paths`: single path  | `CB_NOTIFY` with `notify4_remove_entry` (parent dir + name) and `notify4_change` on the file's FH |
| `invalidate-prefixes`: whole subtree | `CB_NOTIFY` with `notify4_change` on the subtree root, plus `CB_RECALL` if any delegation is held |

`CB_NOTIFY` requires the client to enable directory delegations / change notifications (`OPEN_DELEGATE_*` and `notify4_*` bitmaps). The macOS NFSv4.1 client supports `notify4_change` and `notify4_remove_entry`; the Linux NFSv4.1 client supports both. Windows' NFS client (Services for NFS) is more limited and falls back to attribute-cache expiry — for Windows, we tighten `actimeo` to 5s rather than 86400. This is a per-client mount-flag concern, not a server concern.

The existing `crates/host/src/runtime/invalidation.rs` keeps `record_path` / `record_prefix` and grows a `flush_to_callback_channel(client_session)` next to the existing `fuser::Notifier::inval_entry` path. The two notifier types live behind a small trait so the runtime doesn't care which one is wired.

### D9. Listing batching via `READDIR` with attribute bitmap

NFSv4 `READDIR` accepts a `attr_request` bitmap and returns each entry's attributes inline. We always serve `[type, size, mode, fileid, change, time_modify]` in the entry stream — six attrs covering everything `ls -l` and `stat` need. The result: an `ls -la /github/torvalds` is **one round trip per readdir window** (~100 entries per 8KB chunk), not N+1 round trips like FUSE's default `READDIR` + per-entry `GETATTR`.

This is the largest single performance win versus macFUSE for omnifs's workload. See [§Performance](#performance).

### D10. Sessions with a 64-slot reply cache

Each client establishes one NFSv4.1 session on first contact (`EXCHANGE_ID` + `CREATE_SESSION`). The server allocates a 64-slot reply cache per session, holding the last `OK`/`ERR` response per slot for replay on retry. 64 slots is enough for parallel clients at typical concurrency without bloating memory; the per-slot footprint is bounded by the largest `READ` / `READDIR` reply we generate (capped at 1 MiB).

### D11. No `LOCK` ops; mount with `-o nolocks,locallocks`

NFSv4 byte-range locks are part of the spec but expensive to get right and irrelevant for omnifs (no concurrent writers, no shared access semantics across hosts). The server returns `NFS4ERR_NOTSUPP` for `LOCK` / `LOCKT` / `LOCKU`. Clients are expected to mount with `-o nolocks,locallocks` so the kernel handles flock locally without round trips. Documented in the mount UX.

### D12. AUTH_NONE, restricted to `127.0.0.1`

The server accepts `AUTH_NONE` only — no Kerberos, no `AUTH_SYS` uid/gid checks. This is safe because:

- The TCP socket is bound to `127.0.0.1` (not `0.0.0.0` or `::`); reachable only from the same host.
- `mount_nfs` and `mount.nfs` both default to `AUTH_SYS` and we accept it but ignore the embedded credentials. The exported tree's owner/group is the host process's effective uid/gid (same as today's FUSE behavior; see `inode.rs` `current_uid`/`current_gid`).
- A second user on the same machine could in principle connect, but that is the same threat model as today's FUSE mount with `default_permissions` off.

We document this clearly. If the threat model tightens (multi-user shared host, OS-level VM with multiple guest users), we add `AUTH_SYS` enforcement against `getuid()` of the connecting process via SO_PEERCRED on Linux (and the Mac equivalent via `LOCAL_PEERCRED`). That work is deferred.

## Architecture

### File-level layout

```
crates/host/src/
  nfsd/
    mod.rs          ─ Server, listener, per-connection task spawn
    session.rs      ─ EXCHANGE_ID, CREATE_SESSION, slot reply cache
    compound.rs     ─ COMPOUND op dispatcher (the analog of FuseFs::lookup, etc.)
    ops/
      access.rs     ─ ACCESS
      getattr.rs    ─ GETATTR (attribute serialization)
      lookup.rs     ─ LOOKUP, LOOKUPP, PUTROOTFH, PUTFH, GETFH
      readdir.rs    ─ READDIR (with attr bitmap)
      read.rs       ─ READ
      open.rs       ─ OPEN (read-only), CLOSE
      callback.rs   ─ CB_NOTIFY / CB_RECALL on the back-channel
    fh.rs           ─ File handle encoding/decoding (D6)
    wire.rs         ─ XDR codec, RPC framing (record marking)
  mount.rs          ─ start/stop the nfsd; spawn the kernel mount via mount_nfs/mount.nfs/mount_smbfs-equivalent
```

`crates/host/src/fuse/` is removed. `inode.rs` moves up to `crates/host/src/inode.rs` since both layers (today FUSE, tomorrow NFS) consumed it; the file-level rename clarifies that.

### Crate choice

We don't take a hard dependency on `nfsserve` or any existing crate. `nfsserve` is NFSv3-only and async-trait-shaped in a way that doesn't match our COMPOUND model. We hand-write the protocol on top of a minimal XDR codec (`xdr-codec` crate or a small in-tree implementation) and a record-marked RPC frame reader/writer. The total NFSv4.1 surface we need to implement is bounded by the op list in [§Op coverage](#op-coverage); it is finite and not rapidly evolving (RFC 5661 is from 2010).

This is more work than reusing a crate, but the existing crates were written for serving real disk volumes, not a path-routed virtual filesystem. The impedance mismatch costs more than the spec implementation.

### Op coverage

The minimum viable op set (M0):

| Op                  | Purpose                                          |
| ------------------- | ------------------------------------------------ |
| `EXCHANGE_ID`       | Client identification, session prerequisite     |
| `CREATE_SESSION`    | Establish session + reply cache                 |
| `DESTROY_SESSION`   | Tear down                                       |
| `SEQUENCE`          | Per-COMPOUND ordering, slot accounting          |
| `PUTROOTFH`         | Root FH for the export                          |
| `PUTFH` / `GETFH`   | FH plumbing                                     |
| `LOOKUP` / `LOOKUPP`| Path walk (one component per op)                |
| `GETATTR`           | Attribute fetch                                 |
| `ACCESS`            | Permission probe                                |
| `READDIR`           | Directory enumeration with inline attrs (D9)    |
| `OPEN` (read-only)  | File handle for reads                           |
| `READ`              | Bulk read                                       |
| `CLOSE`             | Release                                         |
| `READLINK`          | Symlink target (used today by inode entries with `EntryKind::Symlink`) |
| `RECLAIM_COMPLETE`  | Session bring-up handshake                      |

Adds in M1 (callback channel):

| Op                  | Purpose                                          |
| ------------------- | ------------------------------------------------ |
| `BIND_CONN_TO_SESSION` | Back-channel bind                            |
| `CB_COMPOUND`       | Outbound callback frame                          |
| `CB_NOTIFY`         | Cache invalidation push                          |
| `CB_RECALL`         | (Used only if/when delegations land — deferred) |

Returns `NFS4ERR_NOTSUPP` for everything outside this list, including all write ops, locking, delegations, layouts, and security ops. Clients gracefully degrade.

### How the new server consumes the existing host

The `compound.rs` dispatcher is a near-mechanical translation of `FuseFs`'s methods. The mapping:

| NFS op    | Existing host call(s)                                             |
| --------- | ----------------------------------------------------------------- |
| `LOOKUP`  | `lookup_check_caches` then `lookup_via_provider` (verbatim)       |
| `GETATTR` | `inodes.get(ino)` → `attr_for_kind` (rename `FileAttr` → NFS attr struct) |
| `READDIR` | `opendir_check_caches` then `opendir_via_provider`; emit `READDIRPLUS`-equivalent inline-attr stream |
| `READ`    | If `backing_path.is_some()` → `pread`; else `runtime.call_read_file(path, offset, len)`. Same branch as `FuseFs::read` today. |
| `OPEN`    | Validate read-only access, allocate a server-side state ID, wrap the existing per-FH `file_cache` |
| `CLOSE`   | Drop the state ID, release the cache entry                        |

The `attr_for_kind` translation: FUSE `FileAttr` and NFS `fattr4` cover the same attributes in different layouts. One conversion function, ~30 lines.

The L0/L2 cache call paths are byte-for-byte the same. The only real change is where the kernel boundary lives.

### Cache invalidation wiring

`crates/host/src/runtime/invalidation.rs` already exposes `cache_delete_path` and `cache_delete_prefix`, and on Linux today drives `fuser::Notifier::inval_entry`. The change is:

```rust
trait MountNotifier: Send + Sync {
    fn invalidate_entry(&self, parent: u64, name: &OsStr);
    fn invalidate_subtree(&self, root: u64);
}
```

Two implementations: a Linux/macFUSE-era `FuseNotifier` (kept temporarily for any leftover Linux-FUSE testing) and `NfsdNotifier` which queues `CB_NOTIFY` payloads onto the per-session back-channel writer task. The runtime holds `Arc<dyn MountNotifier>`; the existing `NotifierHandle` shape (`Arc<Mutex<Option<...>>>`) becomes generic over the trait object.

The flush boundary stays exactly where it is today: `event-outcome` from a provider's `on-event` handler is applied at the response boundary before surfacing the terminal, identical to current behavior.

### Mount UX

The CLI grows two responsibilities the FUSE path didn't have:

1. **Spawn the kernel mount**: after the nfsd binds its port, run the platform mount command.

   - Linux: `mount.nfs4 -o nfsvers=4.1,proto=tcp,port=$PORT,nolocks,locallocks,actimeo=86400,rsize=1048576,wsize=1048576 127.0.0.1:/ $MOUNT_POINT`
   - macOS: `mount_nfs -o nfsvers=4,minorversion=1,tcp,port=$PORT,nolocks,locallocks,actimeo=86400,rsize=1048576,wsize=1048576,readahead=128 127.0.0.1:/ $MOUNT_POINT`
   - Windows: out of scope for v0.3 first cut. The server is reachable; we document `mount -o nolock 127.0.0.1:/ Z:` for users who want to try it. No CLI integration.

2. **Tear down on exit**: SIGTERM → unmount the kernel mount → drain pending callbacks → close the TCP listener → exit. Linux uses `umount`; macOS uses `umount` (no `diskutil` per `AGENTS.md` §Scope).

The CLI surface stays `omnifs mount --mount-point ... --config-dir ... --cache-dir ...`. The user-visible behavior is unchanged: a single command starts everything and blocks until unmount.

### Container story

The Docker image gets simpler. The current image needs `--device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor:unconfined`; the NFS server needs none of that — it binds a TCP port and the in-container kernel mounts via `mount.nfs4`. The Dockerfile drops the `fuse3` package and `/dev/fuse` device requirement; `compose.yaml` drops the `cap_add` and `security_opt` blocks.

For the rshared bind-propagation discussion from the install-UX thread: the NFS path makes that obsolete. The host can mount the in-container NFS server directly by exposing the TCP port (`-p 127.0.0.1:2049:2049`) and running `mount_nfs` on the host pointing at it. No mount-namespace acrobatics, no shared propagation flags. This is a meaningful container-DX win independent of Mac.

## Performance

The performance work is a forcing function on protocol detail. The targets:

- **Hot READ from cache** (file already in L0): ≤ 80 μs end-to-end on Linux loopback, ≤ 120 μs on Mac. macFUSE baseline is ~40 μs and ~60 μs respectively. The 2× gap is the NFS framing cost we accept; it is invisible at the workload level because hot reads on omnifs are a small fraction of wall-clock time anywhere.
- **`ls -la` of a 1000-entry directory** (everything in cache): ≤ 5 ms on Linux, ≤ 8 ms on Mac. macFUSE baseline: ~50 ms (1001 round trips). NFS wins by 6–10× because of inline-attr `READDIR`.
- **`grep -r` over a 5k-file cloned repo (`backing_path`)**: within 1.2× of native-FS baseline on Mac, within 1.05× on Linux. The bound is set by per-op overhead × stat count; aggressive client attr caching brings the second pass to within 1.05× on both OSes.
- **First-byte latency for cold reads**: identical to today (dominated by upstream HTTP/git, ms to seconds). NFS adds < 1 ms.

The tactics that earn these numbers:

1. **D9, D10, D11** above. Inline-attr READDIR alone is most of the listing win.
2. **`TCP_NODELAY` + 1 MiB SO_SNDBUF/SO_RCVBUF** on the loopback socket. Default 8 KiB buffers cap loopback at hundreds of MB/s; 1 MiB lifts it to multi-GB/s, which matters for bulk file reads.
3. **One Tokio task per connection, not per request.** The NFS server reads RPC frames sequentially per connection (RFC 5531 record marking), parses, dispatches the COMPOUND, and writes the reply. Within a COMPOUND, ops are sequential by spec.
4. **Zero-copy reply path for `READ`.** The `READ` reply XDR-encodes a length prefix followed by raw bytes. The server holds onto the `Bytes` from `BrowseCacheL0` and does a vectored write (`writev`) of [header, body] without copying the body buffer.
5. **Reply cache as `Bytes`, not `Vec<u8>`.** The 64-slot per-session cache stores the encoded reply as a refcounted `Bytes`. Replays are zero-copy.
6. **No XDR allocations on the hot path.** Request decoding writes into pre-allocated reusable buffers per connection; reply encoding writes into a per-COMPOUND output buffer that's reused across COMPOUNDs.
7. **`SEQUENCE` slot accounting is lock-free per-session.** Use an atomic bitmap (64-bit word per session) for slot in-use tracking; the slot's reply cache entry sits in a `[ArcSwap<Bytes>; 64]`.

We measure: a `criterion` bench harness in `crates/host/benches/nfsd.rs` exercising COMPOUND_PUTROOTFH+LOOKUP+GETATTR, COMPOUND_PUTFH+READDIR, and COMPOUND_PUTFH+READ against a fixture provider that returns canned in-memory responses. We track these per-PR via a perf check in CI. The numbers above are the v0.3 release gates.

## Cross-platform notes

### Linux

The Linux NFSv4.1 client is the reference implementation. `nordirplus=0` is the default; `actimeo=86400` is honored and works correctly with our callback-driven invalidation. We expect Linux to hit the perf targets first and most easily. The only risk is the kernel's `nfs-utils` version on older distros (RHEL 7-era) — but the project's runtime container is Ubuntu 25.10, so that's not our concern.

### macOS

macOS NFS client is fine but quirkier. Specific points:

- `minorversion=1` must be set explicitly; `nfsvers=4` defaults to 4.0 on Mac and you lose sessions.
- The Mac client respects `actimeo` but its directory cache eviction is more aggressive than Linux's. This is fine — `CB_NOTIFY` is the source of truth.
- Mac doesn't support `noac` semantics cleanly; we never set it.
- File handle staleness on remount surfaces as `Input/output error` rather than a clean unmount; we document that the user must `omnifs unmount` cleanly before relaunching, and we hold the FH stable across in-process restarts (D6's stable-inode work removes this caveat eventually).
- Resource forks and `.DS_Store`: Finder will create `.DS_Store` files on directories it browses, and our exported FS is read-only. The kernel returns `EROFS`, Finder logs it and moves on. We do **not** advertise the AAPL extensions; without them, Finder won't try to create `_._` AppleDouble files.
- Spotlight: explicitly out per the latest design conversation. We don't ship an `mdimporter`. Mounted volumes default to non-indexed.

### Windows

Microsoft's "Services for NFS" client supports NFSv4.1 but with caveats: no callback channel support, weaker UTF-8 handling on file names, and the mount integrates as a drive letter rather than a path. We document this for v0.3 and don't ship CLI integration. WSL2 users can use the Linux client unchanged.

## Test strategy

1. **Wire-protocol fuzzing.** A small `arbitrary`-driven harness builds malformed XDR/COMPOUND frames and feeds them to the server. The server must respond with `NFS4ERR_*` cleanly, never panic, never leak slot reservations.
2. **Conformance against `pynfs`.** [pynfs](https://git.linux-nfs.org/?p=bfields/pynfs.git) is the standard NFSv4.1 conformance suite. We run a curated subset (the read-only ops we implement) in CI. Expected pass rate: 100% of in-scope tests, documented exceptions for `LOCK`/write/delegation tests.
3. **End-to-end via the kernel client.** A new integration test target boots the host with a fixture provider, spawns `mount.nfs4` on Linux CI runners against the loopback port, and asserts `ls`, `cat`, `find -name`, `grep -r` produce expected output. Mac CI is on GitHub-hosted `macos-15` runners.
4. **Benchmark gates** as described in [§Performance](#performance).
5. **Existing host integration test (`runtime_test`).** Stays green unchanged. The kernel mount is not part of that test; the test exercises the router/runtime layer directly. The new layer above gets its own integration test.

## Risks and open questions

### Risks

- **Implementation surface is non-trivial.** RFC 5661 is ~600 pages. Our subset is small (about 18 ops including the callback channel, no state recovery beyond `RECLAIM_COMPLETE`). Estimated effort: 4–6 person-weeks for M0, plus 2 for M1 callbacks, plus 2 for hardening and perf tuning. Real but bounded.
- **Mac client edge cases.** macOS NFS has had bugs around `change` attribute interpretation and `READDIR` cookie stability that occasionally surface as "ls hangs" or "directory listing missing entries." Mitigations: monotonically-increasing `change` attributes per inode (we already do this implicitly via `next_ino`-driven generation); stable `READDIR` cookies derived from `(inode, position_hash)`.
- **Buffer reply cache memory under heavy concurrency.** 64 slots × largest-reply (1 MiB) × N sessions = potentially significant resident memory. Mitigation: cap the per-session cache memory, evict slot replies older than 30 s, and rely on the client retransmit being rare in practice on loopback (it is — TCP doesn't drop on loopback absent OOM).
- **No clear answer for Windows callbacks.** Windows NFS client doesn't reliably honor `CB_NOTIFY`. Mitigation: tighten `actimeo` for Windows clients only; accept that Windows v0.3 is "best effort" behind Linux/Mac.

### Open questions

1. **Should the server re-export the bind-mounted clones via `fs_locations` referrals, even given Mac's rough referral support?** Decision in D7 is no, but worth re-examining if direct passthrough proves slow on big repos.
2. **How do we handle the read-eager-bytes optimization** (where `read_file` returns `FileContent::with_sibling_files`)? The current FUSE path treats sibling files as a cache preload at the L0 layer — same code works under NFS, since the cache layer is shared. No change needed, but worth a unit test confirming the path triggers identically.
3. **Do we want a long-poll `/var/run/omnifs.sock` admin channel** for `omnifs status` to query the server, or is the existing in-process call surface enough? Probably enough; the CLI lives in the same process today.
4. **TLS / RPCSEC_GSS for non-loopback?** Out of scope for v0.3. If we ever expose the server on a non-loopback interface (e.g., LAN access for a team), this becomes a hard requirement. Until then, the bind to `127.0.0.1` is the security boundary.

## Phasing

| Milestone | Scope                                                               | Gate |
| --------- | ------------------------------------------------------------------- | ---- |
| **M0**    | XDR codec, RPC framing, sessions, M0 op set, read-only path through router/runtime, Linux loopback mount working end-to-end on the runtime image. `omnifs mount` works inside the container. | `cargo test --workspace` green; `omnifs mount` + `ls /github/torvalds` produces correct output. |
| **M1**    | Callback channel, `CB_NOTIFY` driven by `event-outcome`, Mac client validation, drop `fuser` dependency. | Mac mount works end-to-end on a `macos-15` GitHub runner; pynfs conformance subset passes; perf targets met. |
| **M2**    | Bench harness, perf tuning to hit the [§Performance](#performance) gates, Windows documentation. | Bench gates wired into CI as a perf check. |
| **M3**    | Container/Dockerfile cleanup (drop `/dev/fuse`, `SYS_ADMIN`); `compose.yaml` simplification; release notes; CHANGELOG.md `[Unreleased]` becomes `[0.3.0]`. | Release-ready. |

Total: ~10–12 person-weeks elapsed for one engineer. Parallelizable across milestones once the protocol skeleton in M0 is in place.

## What this design does **not** do

- It does not implement mutations. Writes are `NFS4ERR_ROFS` until the mutation protocol design lands separately.
- It does not implement file delegations or layouts. A read delegation would let the client cache a file long-term without re-fetching attributes; we revisit if perf telemetry shows a hot file being repeatedly stat-checked.
- It does not implement security (Kerberos, RPCSEC_GSS) or non-loopback access. Bind to `127.0.0.1` is the only access mode.
- It does not target NFSv4.2 features. None are useful here.
- It does not touch the WIT, the SDK, or any provider. The provider authoring surface is unchanged.

## Why not the alternatives, briefly

- **macFUSE bridge first**: deferred and now skipped. Apple's kext deprecation timeline makes it bridge debt with no payoff once we're committed to NFS.
- **FUSE-T**: closed-source userland daemon, personal-use license. Unacceptable runtime dependency.
- **FSKit**: requires app bundle + Developer ID + notarization + system-extension activation prompt. Wrong shape for a CLI tool, and macOS 15+ floor cuts off too many users.
- **microVM (libkrun + NFS export from a Linux guest)**: ships fast but adds a permanent latency floor, ~100 MB resident, and a hypervisor dependency. Per the perf priority, ruled out.
- **SMB3 server**: more featureful for Mac (Finder integration, AAPL extensions, Spotlight), but Spotlight is explicitly out and SMB3 is a heavier protocol surface. NFSv4.1 is the smaller, cleaner spec for our needs.

## Appendix: Why this is the right time

The host architecture as of v0.1 is unusually well-positioned for this swap:

- The protocol-facing layer (`crates/host/src/fuse/`) is ~1300 LOC and depends on the rest of the host through narrow interfaces (`registry.call_*`, `inodes`, `BrowseCacheL0`).
- Cache invalidation is already abstracted from the FUSE notifier — `runtime/invalidation.rs` records paths/prefixes generically and `Linux+FUSE`-only code calls the notifier at the end. Adding an alternative notifier doesn't disturb the runtime.
- File handles, attributes, generation numbers, and file kinds (`EntryKind`) are protocol-agnostic in their current shape.
- The provider-facing WIT is unchanged. Providers continue to compile to `wasm32-wasip2` and load the same way.

In other words, the FUSE-vs-NFS choice was always a choice about the kernel boundary, and the host was structured (perhaps accidentally, perhaps via taste) so that the choice is reversible. We exercise that reversibility now.
