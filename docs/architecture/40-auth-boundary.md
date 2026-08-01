# Auth boundary

Status: current-architecture
Scope: why auth is host-owned and provider-agnostic, and where provider-specific
OAuth facts belong.

Read when: changing credential storage, OAuth flows, auth callouts, provider
endpoint metadata, or capability enforcement.

Binding contracts: `docs/contracts/10-system.md` and
`docs/contracts/50-control-plane.md`. Provider READMEs document vendor-specific
setup facts; they do not bind host behavior.

Providers cannot read the credential store, open a browser, or attach stored
tokens. External access uses host-mediated callouts.

## Principle

The host implements protocols, not vendors.

The host implements OAuth code with PKCE, device flow, static token injection,
storage, refresh, retry, and capability enforcement. Provider metadata and docs
own vendor endpoints, scopes, and API hosts. A new service changes the provider,
not a host vendor table, unless it needs a new protocol family.

## Credential ownership

A Credential resource contains only name, provider, scheme, and account. The
daemon stores material separately, while the CLI owns OAuth and static-token UX
and submits secrets through a durable action on the local control socket. The
daemon injects headers only after a callout crosses the WASM boundary.

Actions use a client ID and generation precondition. The first accepted ID owns
the submitted bytes; retries neither hash nor persist them for dedupe. New
material needs a new ID. Delete and revoke drain serving generations before
removing material; revoke leaves the desired slot empty.

File protection covers accidental exposure and provider escape, not compromise
of the Unix user or trusted host process.

## Auth metadata

Provider metadata declares schemes, injection domains, header shape, flow,
scopes, and setup guidance. The provider macro emits it as
`omnifs.provider-metadata.v1` in Wasm.

Before runtime publication, the daemon binds each mount's credential and
injection facts. The shared credential service owns storage, OAuth transport,
and refresh single-flight; generic auth never branches on provider name.

Provider OAuth details that explain setup or scope belong in
`providers/<name>/README.md`.

## Grants and needs

Metadata declares needs; the resolved mount spec carries grants and is the
runtime authority for domains, auth schemes, preopens, sockets, and other host
effects. Required needs gate mount materialization. Over-grant detection is not
enforced, so the manifest alone does not bound authority.

## Token injection

The host injects tokens only for allowed destinations and configured schemes;
other requests are denied without credentials. Refresh and retry remain host
protocol behavior.

## Runtime trust boundary

The CLI and host-native daemon are trusted; provider WASM is not. Every
filesystem runner is credential-free and attaches only through VFS. The CLI
cannot read daemon SQLite, and the daemon does not consume client-owned desired
state. `OMNIFS_HOME` need not be hidden from its daemon owner; providers and
filesystem guests have no secret or host-resource access.

## Rejected shapes

- host-side vendor tables or `if provider ==` auth branches
- provider-visible stored tokens
- provider-specific OAuth guides in a global guide namespace
- hidden token injection outside declared domains
- claiming the sandbox prevents all exfiltration
