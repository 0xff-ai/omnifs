# Auth boundary

Status: current-architecture
Scope: why auth is host-owned and provider-agnostic, and where provider-specific OAuth facts belong. Binding rules live in `docs/contracts/10-system.md` and provider README files.

Providers are sandboxed WASM components. They cannot read the credential store, open the system browser, or attach stored tokens themselves. External access travels through host-mediated callouts.

## Principle

The host implements protocols, not vendors.

The host can know OAuth authorization code with PKCE, OAuth device flow, static token injection, credential storage, refresh, retry, and capability enforcement. It must not know that a specific provider's authorization endpoint, scope shape, or API host belongs to GitHub, Linear, Slack, or another vendor.

Vendor knowledge lives in provider metadata and provider docs. A new service should require a provider change, not a host table entry, unless the service needs a new protocol family.

## Credential ownership

Providers never hold stored tokens. The host reads and writes credentials under `OMNIFS_HOME`, prepares auth material for host-run callouts, and injects headers only after the callout crosses the WASM boundary.

The provider receives responses or callout-denied errors, not raw credential store access.

Credential file protection is a local desktop trust boundary. It protects against accidental exposure and provider sandbox escape, not compromise of the Unix user or host process.

## Auth metadata

Provider metadata declares auth schemes, injection domains, header shape, flow type, scopes, and setup guidance. Metadata is generated from `#[omnifs_sdk::provider]` annotations and embedded as `omnifs.provider-metadata.v1` during `just providers-build`.

The host extracts that metadata and builds generic auth strategies from it. It does not branch on provider names.

Provider-specific OAuth details belong next to the provider, usually in `providers/<name>/README.md`, when they help a user understand setup or scope consequences.

## Grants and needs

Provider metadata declares needs. The resolved mount spec carries grants. The host materializes the mount only when the grants satisfy required needs.

The resolved spec is the runtime grant authority. It determines allowed domains, auth schemes, preopened host resources, socket access, and other host-mediated authority.

Over-grant detection is still a future policy decision. Until that lands, do not claim the manifest alone bounds authority. The host enforces the resolved spec.

## Token injection

Tokens are injected only for allowed destinations and configured schemes. A provider that asks the host to fetch outside its granted domain set should receive a denied callout, not a credential-bearing request.

Refresh and retry are host protocol behavior. Providers should model permission failures through provider errors or upstream response handling, not by owning refresh tokens.

## Runtime trust boundary

The CLI, daemon, and runtime container are trusted local control-plane code. Provider WASM is untrusted.

Do not design credential boundaries around hiding `OMNIFS_HOME` from the trusted daemon or runtime container when the runtime needs that state. Do design boundaries around preventing provider WASM from reading secrets or escalating host resources.

## Rejected shapes

- host-side vendor tables or `if provider ==` auth branches
- provider-visible stored tokens
- provider-specific OAuth guides in a global guide namespace
- hidden token injection outside declared domains
- claiming the sandbox prevents all exfiltration
