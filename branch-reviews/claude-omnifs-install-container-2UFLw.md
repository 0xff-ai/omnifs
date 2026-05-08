# Review: claude/omnifs-install-container-2UFLw

## Metadata

- Review date: 2026-05-08
- Branch: `origin/claude/omnifs-install-container-2UFLw`
- Head: `69edc5a43e913dd527530eaa22db9925f3f544bc`
- Base: `origin/main` at `7538b346318097c287bcf13889c92a594cc65861`
- Merge base: `7538b346318097c287bcf13889c92a594cc65861`
- Diff reviewed: `git diff origin/main...origin/claude/omnifs-install-container-2UFLw`
- Changed file: `docs/design/nfsv4-loopback-mount.md`

## Overall judgment

Do not accept this as the v0.3 mount-surface plan. The document has a clean goal, replacing FUSE with a kernel-native loopback mount that avoids macFUSE, but it overcommits on NFSv4.1 before proving the two hard parts: kernel-client cache invalidation and container mount reality. The proposal also contradicts this checkout's current Linux-only and Compose-first contract by making macOS support the main driver and by proposing Docker simplification that still requires kernel mount privileges.

The final-shape question is sound: the mount boundary should be swappable while providers, WIT, router, runtime, and caches stay stable. The specific NFSv4.1 design needs to be demoted to a research candidate until a minimal server is validated against real Linux and macOS kernel clients.

## Strengths

- The boundary placement is mostly right. `docs/design/nfsv4-loopback-mount.md:40` through `docs/design/nfsv4-loopback-mount.md:53` keeps the router, inode table, browse caches, runtime, WIT, SDK, and providers below the mount surface.
- The design preserves read-only semantics at `docs/design/nfsv4-loopback-mount.md:36` through `docs/design/nfsv4-loopback-mount.md:38` and does not smuggle mutations into the mount protocol.
- The bind-mounted clone decision at `docs/design/nfsv4-loopback-mount.md:78` through `docs/design/nfsv4-loopback-mount.md:80` is the right instinct: passthrough should remain a server-side implementation detail, not a second mounted export.
- The test strategy calls out real kernel-client and conformance validation, which is necessary for a protocol boundary like this.

## Findings

### Blocking: the cache-invalidation plan depends on callbacks the design has not proven clients will use

`docs/design/nfsv4-loopback-mount.md:82` through `docs/design/nfsv4-loopback-mount.md:93` makes `actimeo=86400` safe by relying on `CB_NOTIFY` and `CB_RECALL` back-channel callbacks for invalidation. That is the core correctness claim of the design. But the doc does not prove that the Linux and macOS kernel clients actually negotiate and honor the required notification classes for this read-only pseudo-filesystem. It also says `CB_RECALL` is used "if any delegation is held" at line 89 while later saying file delegations are not implemented at lines 314 through 315.

If the callback path is not real, the design deliberately tells clients to cache stale provider projections for up to a day. That is not a performance optimization, it is a correctness regression.

Fix shape: make callback support a measured prerequisite, not an assumption. M0 should use conservative TTLs that preserve current behavior. Add a small NFS server proof that records the exact callback negotiation and invalidation behavior for Linux and macOS clients before recommending `actimeo=86400`. If callbacks do not work for this op set, fall back to short TTLs plus explicit host-side invalidation where available.

### Blocking: the container simplification claim is wrong for in-container mounts

`docs/design/nfsv4-loopback-mount.md:229` through `docs/design/nfsv4-loopback-mount.md:233` says the Docker image gets simpler because NFS needs no `/dev/fuse`, `SYS_ADMIN`, or unconfined AppArmor. But the same paragraph says the in-container kernel mounts via `mount.nfs4`. Mounting any filesystem inside the container still requires mount privilege, normally `CAP_SYS_ADMIN`, suitable AppArmor/seccomp policy, and NFS client tooling in the image.

The branch is trying to remove the exact privileges the proposed v0.3 gate at `docs/design/nfsv4-loopback-mount.md:305` still needs. That conflicts with the repo's primary supported workflow, where `docker compose up --build -d` mounts omnifs inside the named container and exposes `/github`, `/dns`, and `/arxiv`.

Fix shape: split the container story into two validated modes. If the container mounts inside itself, keep and document the required mount privileges. If the host mounts a server running inside the container, change the supported workflow explicitly and update `compose.yaml`, ports, health checks, auth/SSH expectations, and demo commands. Do not claim privilege removal until the exact Compose flow has been run.

### High: the design conflicts with the local repo scope

The current workspace guidance says this repo is Linux-only and not to reintroduce macOS-specific mount behavior unless explicitly requested. This design is driven by macOS support at `docs/design/nfsv4-loopback-mount.md:9` through `docs/design/nfsv4-loopback-mount.md:22`, includes macOS CLI mount flags at `docs/design/nfsv4-loopback-mount.md:222`, and uses macOS validation as an M1 gate at `docs/design/nfsv4-loopback-mount.md:306`.

That may be a product direction the project later accepts, but this branch presents it as the v0.3 mount surface without first changing the repo scope. In this checkout, the supported path is Linux container operation.

Fix shape: either reframe the document as cross-platform research outside the current repo-local scope, or make Linux loopback NFS inside the current Compose workflow the only v0.3 scope and keep macOS validation as a separate later milestone after the scope changes.

### High: the implementation estimate understates protocol risk

`docs/design/nfsv4-loopback-mount.md:144` through `docs/design/nfsv4-loopback-mount.md:148` rejects existing crates and proposes hand-writing NFSv4.1, then `docs/design/nfsv4-loopback-mount.md:289` estimates 4-6 person-weeks for M0. The op list at `docs/design/nfsv4-loopback-mount.md:152` through `docs/design/nfsv4-loopback-mount.md:181` includes sessions, state IDs, `OPEN`, `CLOSE`, reply caches, back-channel setup, `READDIR` cookies, and callback plumbing. That is not a small protocol subset once kernel clients are involved.

Fix shape: make the first milestone a throwaway compatibility probe, not the production replacement for FUSE. It should implement only enough to mount, walk, and read a fixture tree under real Linux and macOS clients, then document observed op sequences and kernel quirks. Only after that should the design commit to replacing `crates/host/src/fuse/`.

### Medium: the security model is weaker than the current user-visible mount contract

`docs/design/nfsv4-loopback-mount.md:109` through `docs/design/nfsv4-loopback-mount.md:117` accepts `AUTH_NONE`, ignores `AUTH_SYS`, and says another local user connecting is equivalent to today's FUSE mount with `default_permissions` off. That equivalence is not established. A loopback TCP service is discoverable by other same-host users and is not scoped to the caller's mount namespace in the same way as a FUSE mount owned by one process.

Fix shape: treat same-host multi-user access as part of the first design, not a future tightening. Bind a random high port, write it into a user-private state dir with strict permissions, reject non-loopback, and document whether the server enforces the effective uid. If uid enforcement is not available cross-platform, say the server is single-user by operational contract.

### Medium: the performance numbers are asserted before the implementation exists

`docs/design/nfsv4-loopback-mount.md:237` through `docs/design/nfsv4-loopback-mount.md:254` gives concrete latency and listing targets, then explains the tactics that will earn them. Those are useful goals, but they are written like validated expectations. The design has no measured omnifs NFS prototype, no kernel-client op trace, and no proof that `READDIR` attr batching avoids the `GETATTR` patterns used by the actual client.

Fix shape: label the numbers explicitly as targets and tie acceptance to a benchmark harness plus real mounted client traces. Do not use the claimed macFUSE comparison as justification until there is measured evidence for the exact op mix.

## Validation

I inspected `docs/design/nfsv4-loopback-mount.md` from `origin/claude/omnifs-install-container-2UFLw` with line numbers and compared it against the repo-local Linux/container guidance and current Compose workflow. I ran `git diff --check origin/main...origin/claude/omnifs-install-container-2UFLw`; it reported no whitespace errors. No Rust or Docker checks were run because this branch only adds a design document.

## Recommendation

Do not merge this as the accepted v0.3 mount plan. Keep the useful boundary framing, but rewrite the document as a research proposal with two gates: first prove a minimal NFSv4.1 loopback server against real kernel clients, then prove the exact Docker Compose workflow with the privileges it actually needs. Until those pass, the current Linux FUSE container path remains the supported mount surface.
