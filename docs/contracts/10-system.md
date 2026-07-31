# System contracts

Status: current-contract
Owns: trust boundaries, byte boundaries, provider authority, auth, credentials, and sandbox claims.

## Read when

Read this before touching host/provider trust, callout authority, capabilities, auth metadata, credential storage, OAuth plumbing, sandbox docs, or any claim about what the host or provider may know.

## Rules

### Trust boundary

The host owns trust. Providers are untrusted WASM components. Filesystems expose one trusted host tree to the OS. Upstreams are external systems whose bytes and metadata must be treated as provider input.

Keep credential storage, credential injection, callout execution, cache storage, namespace state, and I/O in the host. Keep provider meaning in the provider: path meaning, object identity, canonical assembly, render, versioning, preload, and revalidation.

Host-owned blob and projection identities come only from validated mount-scoped
request facts. Providers may request a remote, reference, or HTTP payload, but
they never choose a filesystem/cache entry name, and injected credentials are
excluded from those identities. Mount definitions, credentials, provider
artifacts, SQLite state, and cache storage belong to the daemon; the CLI reaches
them only through typed local RPC.

### Byte boundary

The host operates on paths, bytes, content types, file attributes, cache metadata, capability outcomes, and effects. Object meaning stays provider-side.

Lower provider output into neutral host/tree types before filesystem adaptation. Keep canonical bytes opaque to the host. Do not decode canonical object payloads host-side to make projection decisions.

### Provider authority gates

New provider authority is a gated decision. Gate new callout families, new preopens, process effects, socket effects, broader network authority, and auth or transport changes. Describe the security model change and add enforcement-boundary tests in the same change.

Async host imports do not reduce this gate. A provider may suspend on a host import, but the host still owns execution, auth injection, capability checks, timeout behavior, and error mapping. Adding or widening an import is an authority change even when the SDK call site looks like ordinary async Rust.

Provider manifest `capabilities` declare authority needs only: domains, git repos, unix sockets, and preopened paths. Scalar resource ceilings such as memory and blob byte budgets are manifest `limits` and mount-spec `limits`; they must not be described as provider authority or callout grants.

Dynamic domain needs resolve from a provider config field named `domains`, whose string array becomes the mount's concrete HTTP allowlist at startup. Do not use a wildcard domain grant to stand in for this per-mount enumeration.

### Auth and credentials

Credential resources are non-secret desired state in daemon-owned SQLite. Secret material is stored separately and may change only through a typed durable action. Startup resolves each mount auth declaration into one mount-owned binding before namespace publication; that binding loads the material, refreshes OAuth credentials during use, and injects them after a callout crosses the WASM boundary.

Credential material stays out of WIT payloads. It may cross the daemon control boundary only in request-side protobuf messages on the local Unix socket; it never crosses filesystem attach/TCP or appears in responses, status, inventory, logs, Debug, or Inspector output. Route provider auth declarations through provider metadata and mount auth/config resolution. Keep human auth UX in `omnifs-cli`.

OAuth client ids in provider declarations are public application identifiers, not secrets. User access tokens, refresh tokens, and client secrets remain sensitive host-side values. Login, set, refresh, and revoke use client-generated action IDs and action-generation preconditions. The daemon accepts at most one non-terminal action per Credential target, retains it across restart, and never persists or hashes submitted secret bytes for dedupe. The first accepted action ID wins; new material requires a new action ID.

A mount's auth declares identity (Credential resource, scheme, and account), never a sourcing mechanism, so there is no read-from-env or read-from-file path at serve time. Credential deletion and explicit upstream revoke drain every serving generation that can use the material before terminal completion. Revoke leaves the desired Credential slot present and empty.

Credential values must never appear in CLI output, errors, tracing, metrics, or structured envelopes. Source identifiers such as environment-variable names may appear when they make an error actionable.

### Filesystem attach authority

The Docker-hosted filesystem receives no credentials or host filesystem mounts. Its only host authority is the Omnifs VFS wire protocol over a local TCP attach. Docker Desktop reaches a loopback listener through its host forwarder; native Linux reaches a listener bound specifically to the address assigned to the default `docker0` bridge. The daemon validates that interface assignment rather than trusting a caller-supplied address. Do not bind the attach listener on every host interface or give the filesystem host networking merely to cross the container boundary.

The libkrun filesystem guest also receives no credentials or network device. Its only host authority is the three fixed vsock paths for attach, readiness, and keyed ssh. The trusted, signed `omnifs-libkrun` helper owns Hypervisor.framework and libkrun calls; the guest and provider WASM never gain that host authority.

## Must not

- Put provider-specific behavior in `omnifs-engine`, `omnifs-fuse`, or `omnifs-nfs`.
- Claim the sandbox prevents all exfiltration. Allowed network destinations can still be abused by a hostile provider.
- Add provider authority as a side effect of a convenience change.
- Hide a new capability behind a macro argument, manifest field, or config field that is not enforced.
- Transmit credentials through filesystem attach/TCP or expose them in daemon responses, status, inventory, logs, Debug, or Inspector output. Request-side submission on the local Unix control socket is the only allowed crossing.
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
- `crates/omnifs-cli/src/commands/mount/`
- `crates/omnifs-daemon/src/generation_builder.rs`
- `crates/omnifs-cli/src/commands/mount/`
- `providers/*/README.md`

## Validation

- For authority or callout changes, run `just build providers` and host tests that initialize providers.
- For auth changes, test status/readiness output, credential resolution, and the callout path that receives injected auth.
- For WIT or cache boundary changes, add a WIT-boundary or host integration test that asserts lowered bytes, attrs, and effects without provider-specific host decoding.
