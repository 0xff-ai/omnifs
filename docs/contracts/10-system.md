# System contracts

Status: current-contract
Owns: trust boundaries, byte boundaries, provider authority, auth, credentials, and sandbox claims.

## Read when

Read this before changing trust, callout authority, capabilities, auth,
credentials, OAuth, sandbox claims, or the host/provider knowledge boundary.

## Rules

### Trust boundary

The host owns trust; providers are untrusted WASM; upstream bytes and metadata
are provider input. Filesystems expose one trusted host tree to the OS.

The host owns credentials, callouts, storage, namespace state, and I/O.
Providers own path meaning, object identity, canonical assembly, rendering,
versioning, preload, and revalidation.

Host-owned blob and projection identities come only from validated mount-scoped
request facts. Providers may request a remote, reference, or HTTP payload, but
they never choose a filesystem/cache entry name, and injected credentials are
excluded from those identities. Mount definitions, credentials, provider
artifacts, SQLite state, and cache storage belong to the daemon; the CLI reaches
them only through typed local RPC.

### Byte boundary

The host operates on paths, bytes, content types, attrs, cache metadata,
capability outcomes, and effects. Lower provider output into neutral tree types
before filesystem adaptation; never decode canonical objects for host policy.

### Provider authority gates

New callout families, preopens, process or socket effects, broader network
authority, and auth or transport changes are gated. Describe the security
change and test the enforcement boundary.

Async imports do not reduce the gate. The host still owns execution, auth,
capabilities, timeouts, and errors while a provider suspends. Adding or widening
an import changes authority even when its SDK call looks like ordinary Rust.

Manifest `capabilities` declare domain, Git, Unix-socket, and preopen needs.
Scalar ceilings such as memory and blob bytes belong in manifest and mount-spec
`limits`, not authority grants.

Dynamic domain needs resolve from a `domains` config string array into the
mount's startup HTTP allowlist. A wildcard cannot replace this enumeration.

### Auth and credentials

Credential resources are non-secret desired state; secret material is stored
separately and changes only through a typed durable action. Before namespace
publication, startup resolves each mount auth declaration into a binding that
loads, refreshes, and injects material after a callout crosses WASM.

Credential material stays out of WIT. It may cross control only in request-side
protobuf on the local Unix socket, never attach/TCP, responses, status,
inventory, logs, Debug, or Inspector. Provider metadata and mount resolution
own auth declarations; `omnifs-cli` owns human UX.

OAuth client IDs are public application identifiers. Access tokens, refresh
tokens, and client secrets remain sensitive. Login, set, refresh, and revoke
use client IDs and generation preconditions. The daemon retains at most one
non-terminal action per Credential across restart and never hashes or persists
submitted bytes for dedupe. The first accepted ID wins; new material needs a
new ID.

Mount auth declares Credential, scheme, and account, never a serve-time source.
Deletion and revoke drain every generation that can use the material before
completion; revoke leaves the desired slot empty.

Credential values never enter output, errors, tracing, metrics, or structured
envelopes. Source names may appear when they make an error actionable.

### Filesystem attach authority

Docker filesystems receive no credentials or host mounts. Their only host
authority is VFS over local TCP: Docker Desktop uses its host forwarder; native
Linux uses the validated default `docker0` address. Never trust a caller address,
bind all interfaces, or grant host networking for attach.

The libkrun guest has no credentials or network device. Its only host authority
is fixed vsock attach, readiness, and keyed SSH. The trusted signed helper alone
owns Hypervisor.framework and libkrun calls.

## Must not

- Put provider-specific behavior in `omnifs-engine`, `omnifs-fuse`, or `omnifs-nfs`.
- Claim the sandbox prevents all exfiltration. Allowed network destinations can still be abused by a hostile provider.
- Add provider authority as a side effect of a convenience change.
- Hide a new capability behind a macro argument, manifest field, or config field that is not enforced.
- Transmit credentials through attach/TCP or expose them in responses, status,
  inventory, logs, Debug, or Inspector. Only request-side local control may
  carry them.
- Let providers read the credential store directly.
- Build a provider-specific credential bypass in host runtime code.
- Treat WIT async imports as provider-owned I/O.

## Code

- `crates/omnifs-wit/wit/provider.wit`
- `crates/omnifs-engine`
- `crates/omnifs-engine/src/callouts/mod.rs`
- `crates/omnifs-engine/src/callouts/http.rs`
- `crates/omnifs-auth`
- `crates/omnifs-state/src/credential.rs`
- `crates/omnifs-provider/src/manifest.rs`
- `crates/omnifs-daemon/src/generation_builder.rs`
- `crates/omnifs-cli/src/commands/mount/`
- `providers/*/README.md`

## Validation

- Authority or callout changes: build providers and run host initialization
  tests.
- Auth changes: test status/readiness, credential resolution, and injected
  callouts.
- WIT or cache boundaries: test lowered bytes, attrs, and effects without
  provider-specific host decoding.
