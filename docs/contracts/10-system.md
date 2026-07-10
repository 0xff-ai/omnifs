# System contracts

Status: current-contract
Owns: trust boundaries, byte boundaries, provider authority, auth, credentials, and sandbox claims.

## Read when

Read this before touching host/provider trust, callout authority, capabilities, auth metadata, credential storage, OAuth plumbing, sandbox docs, or any claim about what the host or provider may know.

## Rules

### Trust boundary

The host owns trust. Providers are untrusted WASM components. Frontends expose one trusted host tree to the OS. Upstreams are external systems whose bytes and metadata must be treated as provider input.

Keep credential storage, credential injection, callout execution, cache storage, namespace state, and I/O in the host. Keep provider meaning in the provider: path meaning, object identity, canonical assembly, render, versioning, preload, and revalidation.

### Byte boundary

The host operates on paths, bytes, content types, file attributes, cache metadata, capability outcomes, and effects. Object meaning stays provider-side.

Lower provider output into neutral host/tree types before frontend adaptation. Keep canonical bytes opaque to the host. Do not decode canonical object payloads host-side to make projection decisions.

### Provider authority gates

New provider authority is a gated decision. Gate new callout families, new preopens, process effects, socket effects, broader network authority, and auth or transport changes. Describe the security model change and add enforcement-boundary tests in the same change.

Async host imports do not reduce this gate. A provider may suspend on a host import, but the host still owns execution, auth injection, capability checks, timeout behavior, and error mapping. Adding or widening an import is an authority change even when the SDK call site looks like ordinary async Rust.

Provider manifest `capabilities` declare authority needs only: domains, git repos, unix sockets, and preopened paths. Scalar resource ceilings such as memory and blob byte budgets are manifest `limits` and mount-spec `limits`; they must not be described as provider authority or callout grants.

Dynamic domain needs resolve from a provider config field named `domains`, whose string array becomes the mount's concrete HTTP allowlist at startup. Do not use a wildcard domain grant to stand in for this per-mount enumeration.

### Auth and credentials

Credentials live host-side. Providers declare auth needs; the host resolves, stores, refreshes, and injects credentials after a callout crosses the WASM boundary.

Keep credentials out of WIT payloads and daemon REST payloads. Route provider auth declarations through provider metadata and mount auth/config resolution. Keep human auth UX in `omnifs-cli`.

OAuth client ids in provider declarations are public application identifiers, not secrets. User access tokens, refresh tokens, and client secrets remain sensitive host-side values. `omnifs init` owns first-run OAuth mount generation; `omnifs init --reauth <mount>` owns repair and re-authentication. Credentials live only in the host credential store: a mount's auth declares identity (scheme, account) and never a sourcing mechanism, so there is no read-from-env or read-from-file path at serve time.

### Frontend attach authority

The Docker-hosted frontend receives no credentials or host filesystem mounts. Its only host authority is the token-authenticated namespace wire. Docker Desktop reaches a loopback listener through its host forwarder; native Linux reaches a listener bound specifically to the address assigned to the default `docker0` bridge. The daemon validates that interface assignment rather than trusting a caller-supplied address. Do not bind the attach listener on every host interface or give the frontend host networking merely to cross the container boundary.

## Must not

- Put provider-specific behavior in `omnifs-engine`, `omnifs-fuse`, or `omnifs-nfs`.
- Claim the sandbox prevents all exfiltration. Allowed network destinations can still be abused by a hostile provider.
- Add provider authority as a side effect of a convenience change.
- Hide a new capability behind a macro argument, manifest field, or config field that is not enforced.
- Transmit credentials through the daemon API.
- Let providers read the credential store directly.
- Build a provider-specific credential bypass in host runtime code.
- Treat WIT async imports as provider-owned I/O.

## Code

- `crates/omnifs-wit/wit/provider.wit`
- `crates/omnifs-engine`
- `crates/omnifs-engine/src/callouts/mod.rs`
- `crates/omnifs-engine/src/callouts/http.rs`
- `crates/omnifs-auth`
- `crates/omnifs-workspace/src/creds`
- `crates/omnifs-caps`
- `crates/omnifs-workspace/src/authn/resolve.rs`
- `crates/omnifs-workspace/src/mounts/materialize.rs`
- `crates/omnifs-cli/src/commands/auth`
- `crates/omnifs-cli/src/commands/init`
- `providers/*/README.md`

## Validation

- For authority or callout changes, run `just providers build` and host tests that initialize providers.
- For auth changes, test status/readiness output, credential resolution, and the callout path that receives injected auth.
- For WIT or cache boundary changes, add a WIT-boundary or host integration test that asserts lowered bytes, attrs, and effects without provider-specific host decoding.
