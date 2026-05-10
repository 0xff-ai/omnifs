# NFS loopback frontend status and implementation path

Status: NFSv4.0 is the primary implementation target

Branch: `raulk/nfs-loopback-design`

## Verdict

The implementation target is a focused read-only NFSv4.0 loopback frontend.
The current NFSv4.1 experiment proved a Linux client can mount a synthetic
fixture, but it is not the straight implementation path for this product. This
macOS 15.6 workstation rejects `vers=4.1`; the supported native macOS protocol
floor is NFSv4.0. The production Linux path remains FUSE.

NFSv3 remains useful as an oracle and compatibility datapoint because native
Windows Client for NFS is documented as NFSv2/v3. It is not the primary
implementation track unless the product goal changes from NFSv4 to built-in
native Windows NFS at any cost.

Do not use `embednfs` as an implementation dependency. It is an NFSv4.1
experiment with too little maintenance and adoption for this boundary, and it
does not match the macOS 15 protocol floor. NFSv4.0 is not NFSv4.1 with
`minorversion = 0`; it has different client-id, lease, and open-owner flows.

## Evidence

The important facts are:

- Docker/Linux passed `mount.nfs4 -o vers=4.1,...` against the synthetic
  read-only fixture, including `ls`, `stat`, `cat`, `find`, recursive reads,
  read-only write failure, and clean `umount`.
- macOS 15.6 rejected `mount_nfs -o vers=4.1,...` during option handling.
- The local macOS `mount_nfs(8)` page says NFSv4 minor version zero is the
  highest supported minor version.
- The local macOS `nfs(5)` page lists NFSv2, NFSv3, and NFSv4 as client
  support and describes NFSv4 as RFC 3530.
- Apple open-source `NFS-327.140.2` caps NFSv4 minor support at zero. Later
  Apple `NFS-339` source documents NFSv4.1 client support.
- Microsoft's current NFS overview documents native Windows Client for NFS as
  NFSv2/v3, while NFSv4.1 is listed for Server for NFS.
- `anylinuxfs` is useful evidence for the macOS NFS approach, but it uses a
  Linux microVM and Linux NFS services. It is a design reference, not code to
  copy.
- A Linux kernel `nfsd` oracle now passes a Docker/Linux NFSv4.0 mount against
  the synthetic read-only fixture with `ls`, `stat`, `cat`, `find`, recursive
  traversal, hashes, read-only write failure, and clean `umount`.
- The same oracle now passes a Docker/Linux NFSv3 mount with explicit NFS and
  MOUNT ports. The NFSv3 client trace includes `GETATTR`, `LOOKUP`, `ACCESS`,
  `READLINK`, `READ`, `READDIRPLUS`, `FSSTAT`, `FSINFO`, and `PATHCONF`, with
  no write RPCs before the read-only write probe fails locally as expected.
- A standalone Rust NFSv4.0 synthetic fixture now passes the Docker/Linux
  kernel client gate without `embednfs`. The client negotiated v4.0 with
  `SETCLIENTID` and `SETCLIENTID_CONFIRM`, not v4.1 sessions, and exercised
  `PUTROOTFH`, `GETFH`, `GETATTR`, `READDIR`, `LOOKUP`, `OPEN`, `READ`,
  `CLOSE`, and `READLINK`.
- The standalone fixture returned `NFS4ERR_ROFS` for the read-only write probe
  and cleanly unmounted.
- The standalone fixture also passes a Docker/Linux restart/stale-handle gate:
  after server generation changes, old handles fail with `Stale file handle`,
  clean unmount still works, and a fresh remount recovers.
- The oracle also proved an NFSv4 grace-period trap: with the default kernel
  grace window, `cat` stalled while the server returned NFS status `10013`
  (`NFSERR_GRACE`). Kernel-oracle validation must shorten or otherwise
  account for `nfsv4gracetime` and `nfsv4leasetime` during deterministic
  synthetic bring-up.
- macOS `mount_nfs` has not been run from Codex yet because the local
  environment has no non-interactive sudo. Validation should fail closed before
  any macOS mount attempt unless it can use `sudo -n mount_nfs` and
  `sudo -n umount`.
- macOS `mount_nfs` was run from a normal user shell with cached sudo against
  both the kernel `nfsd` oracle and the standalone Rust fixture. Both failed
  with `Invalid argument` before any NFS packet reached the server because the
  NFSv4 option set included `soft` and `locallocks`. Local parser probes show
  macOS 15.6 rejects those options with `vers=4`; use an external timeout
  around `mount_nfs` instead of those options.
- With those fixed options, macOS 15.6 established an NFSv4.0 mount against the
  kernel `nfsd` oracle. An intermediate validation run failed before shell
  probes because macOS reports `/var/...` temp mountpoints as
  `/private/var/...`; validation must canonicalize mount detection and cleanup.
- The clean macOS kernel `nfsd` rerun passed: `mount_nfs` established an
  NFSv4.0 read-only mount, `ls`, `stat`, `cat`, `find`, SHA-256 reads, and the
  read-only write-failure probe all behaved as expected, and cleanup reported
  `umount_ok`.
- The integrated Omnifs NFSv4.0 backend now passes the Docker/Linux kernel
  client gate over the test provider on the latest code. It mounts read-only,
  lists, stats, cats, traverses, hashes, rejects a write-shaped open as
  read-only, unmounts cleanly, and verifies the NFSv4.0 trace.
- The same integrated backend now passes real macOS `mount_nfs` over the test
  provider after the runtime cache-scope fix. The run mounted read-only on
  loopback, listed `/test`, read projected files, tightened
  `/test/hello/message` to `size=13` after open/read, traversed `/test/hello`
  and `/test/scoped`, rejected a write-shaped open as read-only, unmounted
  cleanly, and verified the trace.
- The first real-provider Docker/Linux NFSv4.0 run exposed a real runtime cache
  bug: lookup-side projection caching wrote the root sibling set under the
  looked-up child path for implicit dynamic-prefix directories. Looking up
  `/arxiv/papers` poisoned `/arxiv/papers`, so `/arxiv/papers/1706.03762`
  returned `NFS4ERR_NOENT` before the provider was called.
- After scoping lookup sibling caches correctly, the DNS/arXiv Docker/Linux
  real-provider gate passed: Cloudflare `NS`, arXiv `metadata.json`, traversal,
  read-only failure, clean unmount, and trace verification all succeeded.
- The DNS/arXiv macOS real-provider gate now also passes with real
  `mount_nfs`: the mounted root lists `arxiv` and `dns`, Cloudflare `NS` and
  arXiv `metadata.json` read correctly, traversal and read-only failure pass,
  cleanup reports `umount_ok`, and the NFSv4.0 operation sequence matches the
  expected read-only flow.
- The GitHub/tree-ref Docker/Linux gate now passes as a separate opt-in probe:
  it mounts `:/omnifs`, resolves `/github/octocat/Hello-World/_repo`, clones
  through the existing SSH-backed tree-ref passthrough path, reads `README`,
  rejects a write-shaped open as read-only, unmounts cleanly, and verifies the
  trace. The macOS GitHub/tree-ref gate now also passes with real `mount_nfs`:
  it mounts `127.0.0.1:/omnifs`, browses `octocat/Hello-World`, clones through
  `_repo`, reads `README`, rejects a write-shaped open as read-only, unmounts
  cleanly, and records the expected read-only NFSv4.0 operation sequence.
- The integrated backend exposed an NFS-specific projected-size constraint:
  updating inode size after `READ` is too late for the Linux NFS client when a
  file was advertised with the 256 MiB Omnifs placeholder. The backend now
  materializes provider-backed files during NFS `OPEN`, before the client
  observes post-open attributes.

Primary sources to keep open while implementing:

- NFSv4.0: <https://www.rfc-editor.org/rfc/rfc7530>
- NFSv4.0 XDR: <https://www.rfc-editor.org/rfc/rfc7531>
- ONC RPC: <https://www.rfc-editor.org/rfc/rfc5531>
- XDR: <https://www.rfc-editor.org/rfc/rfc4506>
- NFSv3: <https://www.rfc-editor.org/rfc/rfc1813>
- macOS NFS source: <https://github.com/apple-oss-distributions/NFS>
- NFS-Ganesha: <https://nfs-ganesha.github.io/>

## Target architecture

The final architecture remains parallel frontends over the same host read
model:

```text
Linux
  CLI mount -> FUSE frontend -> router -> inode table -> browse caches -> runtime -> providers

macOS first, native Windows only if a v4-capable client is identified
  CLI mount -> NFS frontend -> router -> inode table -> browse caches -> runtime -> providers
```

The Linux FUSE and container workflow stays the production Linux path. NFS must
not delete, weaken, or replace the existing FUSE/container workflow.

The NFS frontend is read-only for this phase. All write-shaped protocol
operations return read-only or not-supported errors. Mutation work remains out
of scope.

## Straightest path

The staged NFSv4.0 protocol boundary has passed Linux kernel `nfsd`,
Docker/Linux NFSv4.0, Docker/Linux NFSv3, Docker/Linux standalone Rust fixture,
stale-handle, macOS kernel `nfsd` NFSv4.0, macOS standalone Rust NFSv4.0,
Docker/Linux integrated Omnifs backend, macOS integrated Omnifs backend,
Docker/Linux plus macOS DNS/arXiv real-provider gates, and Docker/Linux plus
macOS GitHub/tree-ref passthrough gates.

The production code path now lives in the `omnifs-nfs` crate and the CLI mount
wiring. Keep future protocol probes and packet captures outside the production
tree unless they are intentionally promoted as maintained tests. The important
validation properties are the client, mount options, command output, server
trace, operation summary, stale-handle behavior, read-only write failure, and
clean unmount result.

The integrated Omnifs NFS frontend accepts the named export path
`127.0.0.1:/omnifs` as an alias for the provider root. This removes the extra
`omnifs` directory from the mounted filesystem: the exported NFS root itself is
the Omnifs provider root. Local `mount_nfs(8)` and `mount(8)` docs do not
expose a supported NFS option for overriding the displayed server/source name,
so Finder may still show a loopback source such as `127.0.0.1`. The
product-facing name should come from the user-visible mountpoint, normally a
directory named `omnifs`, while the protocol source uses `:/omnifs` to avoid
the nested-directory UX.

The macOS standalone Rust pass also exposed normal macOS negative lookup
traffic: the client asks for `._.` resource-fork metadata entries during shell
traversal and for `new-file` during the read-only write probe. The server
returns `NFS4ERR_NOENT` for those terminal `LOOKUP` compounds. The integrated
Omnifs backend later exposed another real macOS behavior: recursive traversal
can issue `READDIR` against a file-shaped provider route. That must not surface
as generic IO. The NFS adapter maps provider read results from a list request
to `NFS4ERR_NOTDIR`.

Do not add `soft` or `locallocks` to NFSv4 macOS mounts. macOS 15.6 rejects
that option set with `EINVAL` before contacting the server.

If native Windows remains a hard requirement, also keep validating the NFSv3
path because that is the Windows native client path. Do not run Finder,
Spotlight, aggressive TTL, or callback-invalidation tests yet. Real provider
tests should start with the narrowest safe provider/cache surface and only then
expand toward GitHub, DNS, arXiv, and tree-ref passthrough reads.

## Decision table

| Evidence | Decision |
| --- | --- |
| Linux kernel `nfsd` NFSv4.0 fails on macOS | Do not build an Omnifs NFSv4.0 server. Test NFSv3 or switch frontend. |
| Linux kernel `nfsd` NFSv4.0 passes on macOS | NFSv4.0 is viable for macOS. Continue with the focused v4.0 read-only server. |
| Standalone Rust NFSv4.0 fixture fails on macOS after Linux passes | Debug the v4.0 implementation before any Omnifs runtime integration. |
| Linux kernel `nfsd` NFSv3 passes on macOS and Windows | Record native Windows compatibility evidence. Do not switch production away from NFSv4 without a product decision. |
| Any client hangs or cannot cleanly unmount | Treat as blocking until recovery is deterministic. |
| Callback invalidation is absent or unreliable | Keep conservative TTLs. Do not enable aggressive attr caching. |

## NFSv3 compatibility track

NFSv3 is not the main implementation track. Keep it as a compatibility oracle
for native Windows and as a comparison point for client behavior. The useful
parts are explicit high NFS and MOUNT ports, read-only failure expectations,
recorded client traces, and mount/unmount discipline. Do not add a Rust NFSv3
frontend unless the product requirement changes.

## NFSv4.0 implementation track

This is the cleaner macOS NFS track, but it is not the shortest native Windows
track.

A focused read-only NFSv4.0 server must implement:

- ONC RPC v2 over TCP record marking.
- XDR big-endian 4-byte aligned encoding and decoding.
- NFS program `100003`, version `4`, procedures `NULL=0` and `COMPOUND=1`.
- `COMPOUND` with `minorversion == 0`, current filehandle, saved filehandle,
  ordered operation execution, and first-error stop behavior.
- Pseudo-root namespace using `PUTROOTFH` and `LOOKUP`.
- Opaque file handles with truthful expiration semantics.
- `PUTROOTFH`, `PUTFH`, `GETFH`, `GETATTR`, `LOOKUP`, `LOOKUPP`, `ACCESS`,
  `READDIR`, `READ`, `READLINK`, `SAVEFH`, `RESTOREFH`, and `SECINFO`.
- `SETCLIENTID`, `SETCLIENTID_CONFIRM`, `RENEW`, `OPEN`, `OPEN_CONFIRM` if
  requested, and `CLOSE`.
- Read opens only, `OPEN_DELEGATE_NONE`, and no callback-dependent correctness.
- Exact `supported_attrs`, including enough attributes for real `stat` and
  directory traversal.
- `READDIR` cookies that are stable for a directory snapshot and never use
  reserved cookie values `0`, `1`, or `2`.
- `NFS4ERR_ROFS` for mutating operations.

Do not grant delegations in the first version. Mount macOS with `nocallback`,
`noac`, and `nonegnamecache` during protocol bring-up. Only test callback
invalidation after mount, traversal, read, read-only failure, stale-handle, and
unmount behavior are stable.

## Rejected dependency

The earlier branch experiment used `embednfs` for an NFSv4.1-oriented server
path. That dependency remains rejected. It is too unmaintained for this
boundary, does not match the macOS 15 NFSv4.0 support floor, and should be
treated as historical evidence only.

The current host NFS module is a direct read-only NFSv4.0 implementation that
graduated only after the standalone Rust fixture passed the Docker/Linux and
macOS gates. Keep it isolated as a mount backend over the existing registry,
runtime, browse caches, and provider stack. Do not let it replace the Linux
FUSE/container path.

## Safety ladder

The staged gate order is strict:

1. Linux kernel `nfsd` synthetic fixture.
2. Linux NFS client control inside Docker, fresh empty temp mountpoint,
   read-only mount, shell probes only, clean `umount`.
3. Standalone Rust NFSv4.0 synthetic fixture, Linux NFS client control inside
   Docker, fresh empty temp mountpoint, read-only mount, shell probes only,
   clean `umount`.
4. macOS `mount_nfs` against the kernel `nfsd` fixture on `127.0.0.1`, fresh
   empty temp mountpoint, shell probes only, clean `umount`.
5. macOS `mount_nfs` against the standalone Rust NFSv4.0 fixture on
   `127.0.0.1`, fresh empty temp mountpoint, shell probes only, clean `umount`.
6. Native Windows NFS client only as compatibility evidence; for native NFSv4,
   first identify a specific v4-capable native client.
7. Omnifs read-only fixture runtime using NFSv4.0, first Docker/Linux and then
   macOS.
8. Real providers, only after synthetic and Omnifs fixture gates pass.
9. Conservative TTL profile first.
10. Callback invalidation and aggressive TTLs only after basic behavior is
   stable and measured.

If any mount cannot be unmounted cleanly, stop and document recovery before
continuing.

## What must be proven

For each target client, record real commands, output, and server traces for:

- Mountability on loopback.
- Clean and failed unmount behavior.
- `ls`, `stat`, `cat`, `find`, and recursive traversal.
- Attribute request patterns and whether directory entry attrs reduce lookup
  storms.
- Conservative attribute caching and negative lookup behavior.
- Stale handles after generation changes, restart, removal, and remount.
- Read-only failures for write-shaped operations.
- Auth flavor, uid, gid, group list, owner strings, and local-user isolation.
- Whether callback invalidation is negotiated and observed.

Callback invalidation is not assumed. Aggressive TTLs require evidence that the
client invalidates warmed `ls`, `stat`, `cat`, and `find` results within one
second after server-side fixture changes. If that is not proven per client, use
short conservative TTLs and measure the cost.

## Auth and security model

The initial security model is localhost-only and single-user-oriented:

- Bind to `127.0.0.1` by default.
- Use random high ports where the client permits it.
- Store mount state under a user-private directory with `0700` permissions.
- Prefer a nonce-bearing export path if the client transmits the export path
  early enough for the server to reject unauthorized attempts.
- Treat `AUTH_SYS` uid/gid as advisory unless it can be bound to the local OS
  user.

Do not claim equivalence with FUSE permission isolation until a same-host
second-user probe proves it. Do not bind non-loopback addresses by default.

## Windows contract

Native Windows support requires testing native Windows Client for NFS.
With Microsoft's documented client support, the native Windows NFS path is
NFSv3 unless a different v4-capable client is identified.
