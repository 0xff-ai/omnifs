# Host authentication, OAuth, and credential storage

Status: accepted; current implementation covers static-token auth, generic OAuth plumbing, GitHub device-code OAuth, and Linear PKCE OAuth.
Scope: `omnifs.provider-metadata.v1` custom section (`auth` block in `omnifs.provider.json`), `crates/omnifs-mount-schema`, `crates/omnifs-sdk`, `crates/omnifs-host/src/auth.rs`, `crates/omnifs-host/src/http.rs`, `crates/omnifs-cli`, `crates/omnifs-auth`, `crates/omnifs-creds`.

## Context

omnifs providers run sandboxed as `wasm32-wasip2` components. They cannot speak to the network, the OS keychain, or a system browser directly. Every external call rides a host-mediated callout (`fetch`, `fetch-blob`, `git-open-repo`, `ws-*`). The host is the only place credentials live and the only place token acquisition can happen.

Each provider design under `docs/design/providers/` punts on this question, declaring `auth_types: ["oauth2"]` or `["bearer-token"]` and assuming the host will "inject the right header". This document specifies what that means.

The patterns the host must support, in order of how much work they imply:

1. Static bearer or API-key tokens pasted into instance config: Linear API keys, Notion internal-integration secrets, GitHub PATs, Cloudflare API tokens, Slack bot tokens.
2. OAuth 2.1 authorization-code + PKCE with loopback redirect (RFC 8252 BCP for native apps): Linear public OAuth, Notion public integrations, Slack OAuth v2 (with one HTTPS-redirect caveat), Google Workspace.
3. OAuth 2.0 device authorization grant (RFC 8628): GitHub OAuth Apps, which can be tested with only a client id, so the GitHub provider uses device flow instead of a callback.
4. Service-shaped credentials that are not OAuth: Postgres connection strings, kubeconfig with exec providers.

## Load-bearing principle: the host is provider-agnostic

The host implements **protocols**, never **vendors**. The host knows what OAuth 2.1 + PKCE and OAuth device flow are, how to bind a loopback listener, how to drive a system browser or print a device code, how to store and refresh tokens, and how to retry a 401. The host does not know that Linear's authorize endpoint is `https://linear.app/oauth/authorize`, that GitHub's device endpoint is `https://github.com/login/device/code`, or that Slack's `redirect_uri` must be HTTPS. All vendor-specific knowledge lives in the provider, which is the right place: it already knows the upstream API, parses its responses, and rate-limits against its quotas.

This is the same architectural rule that keeps the rest of omnifs clean. The host knows FUSE, caching, capability sandboxing, and the WIT protocol; it does not know "GitHub issues" or "DNS records".

Consequences:

- No per-provider table, switch, match, or `if provider == "linear"` anywhere in host code.
- Adding a new service is "write a new provider"; it never requires a host release unless the service needs a new protocol family.
- The host's OAuth implementation is testable as generic PKCE and device-flow clients against RFC-conforming authorization servers, without booting any specific provider.

## Goals

Make OAuth a first-class authentication path with the same UX MCP servers offer today: `omnifs auth login <mount>` either opens a browser for PKCE or prints a device-code URL and user code, then the mount works after authorization.

Keep credentials out of the provider sandbox. Providers never see tokens. They get authenticated HTTP responses and `denied` callout errors; everything else is the host's concern.

Keep vendor knowledge inside providers. The host's OAuth engine reads metadata supplied by the provider's auth manifest and optionally overridden by the instance config. The engine does not branch on which provider it is serving.

Store tokens securely. macOS Keychain via the Security framework, Linux libsecret via the Secret Service API, Windows DPAPI via the credential vault. File fallback at `~/.omnifs/data/credentials.json` mode 600 for headless environments (CI, containers without a session bus).

Refresh transparently. A 401 inside a provider callout triggers a file-lock-protected refresh and one retry; the provider observes a successful response on the second attempt or a `permission-denied` callout error if refresh fails.

Make the abstraction pluggable. OAuth and static bearer are host HTTP-auth strategies. Non-HTTP credentials, such as DSNs and file paths, stay in the provider's instance config, outside the host auth layer and OAuth credential store.

## Non-goals

Federated SSO, SAML, or enterprise IdP integration as a primary code path. Users authenticate through the vendor's OAuth surface. If a workspace federates, the browser handles it.

Multi-tenant credential isolation. omnifs is a single-user desktop daemon. One Unix user, one credential vault.

DPoP, mTLS bearer binding, or other proof-of-possession schemes. A bearer token in the OS keychain is the strength level this design targets. If a future provider demands DPoP, it gets its own design extension.

Server-side or agentless OAuth beyond the standard device-code grant. The host can run RFC 8628 device flow when the provider declares it, but it does not run a hosted broker service or hold provider secrets.

Auth for the host's own admin RPC (CLI ↔ daemon control channel). The daemon trusts requests from the same Unix user.

## Threat model

Defended:

- **Disk exfiltration of static tokens.** OS keychain is the primary storage; file fallback is mode 600 and warns at startup. An attacker reading the home directory without the owning UID's session does not get tokens.
- **Token leakage through provider sandbox.** The provider cannot read host memory, the file store, or the keychain. The only token exposure surface is the `Authorization` header on the outgoing HTTP request, which the provider does not see (the host attaches it after the callout crosses the WIT boundary).
- **Mis-targeted token injection.** Tokens are scoped to a domain set declared in capabilities; the host refuses to inject on URLs outside that set. A provider asking the host to fetch `https://attacker.example/` does not receive the GitHub token.
- **Refresh races.** A cross-process file lock ensures that N concurrent 401s do not produce N parallel refresh requests (some vendors throttle aggressively and rotate refresh tokens on use, which invalidates parallel callers).

Not defended:

- **Compromise of the host process or the Unix user.** Anyone with code execution as the owning user can read the keychain (with at most a one-time approval prompt on macOS). That is the standard desktop trust boundary.
- **Compromise of the vendor's OAuth surface.** Outside our control.
- **Stolen refresh tokens.** Once stolen, they can be used until revoked. `omnifs auth logout --revoke` revokes server-side when the provider declares a revocation endpoint.

## Where vendor knowledge lives: the auth manifest

The provider declares its HTTP authentication needs in the `auth` block of `providers/<name>/omnifs.provider.json`, embedded in the wasm `omnifs.provider-metadata.v1` section. The host extracts provider metadata once at provider load, derives the runtime `AuthManifest` via `ProviderManifest::wasm_auth_manifest()`, and caches the result for the lifetime of the mount.

```text
auth manifest:
  schemes: list<AuthScheme>

AuthScheme:
  none
  staticToken(StaticTokenScheme)
  oauth(OauthScheme)

StaticTokenScheme:
  key: string
  headerName?: string
  valuePrefix: string
  description: string
  injectDomains: list<string>

OauthScheme:
  key: string
  displayName: string
  authorizationEndpoint: string
  tokenEndpoint: string
  revocationEndpoint?: string
  defaultClientId?: string
  defaultScopes: list<string>
  flow: OAuthFlow
  tokenEndpointAuth: TokenEndpointAuthMethod
  refreshTokenRotates: bool
  extraAuthorizeParams: list<KeyValue>
  extraTokenParams: list<KeyValue>
  injectDomains: list<string>
  injectHeaderName?: string
  injectValuePrefix: string

OAuthFlow:
  pkceLoopback(PkceLoopbackConfig)
  pkceManualCode(PkceManualCodeConfig)
  deviceCode(DeviceCodeConfig)
```

Manifest validation is schema-backed at provider load. The schema is stricter than `"format": "uri"` for OAuth endpoints:

- `authorizationEndpoint`, `tokenEndpoint`, `revocationEndpoint`, and `deviceAuthorizationEndpoint` must match `^https://`.
- `pkceLoopback.redirectUriTemplate` must match `.*\{port\}.*`.
- `pkceManualCode.redirectUri` must not contain `{port}`.
- `injectDomains` entries are hostnames, not URLs; the host rejects schemes, paths, and wildcards.

The manifest's scheme list expresses the HTTP auth options the provider supports. The instance config picks one (by `auth.scheme = "<key>"` for OAuth, or by `auth.type = "static-token"` for the static path). If the manifest is absent or its scheme list is empty, the provider needs no HTTP credentials and the host injects nothing.

The provider owns:

- which OAuth endpoints exist
- what scopes the upstream API needs by default
- which header carries the token and how it is formatted
- which hosts are valid injection targets
- which flow shape works against this vendor

The host owns:

- the OAuth PKCE and device-flow state machines
- the loopback listener
- the system browser launch
- the device-code prompt
- the manual-code CLI prompt
- the token store
- the refresh + 401-retry path
- the credentials file lock

Instance-config overrides are explicit fields, not vendor-specific carve-outs. Providers carry product client ids in their auth manifests when they can use a public-client flow; the implementation also supports fields for BYO OAuth apps:

- `clientId` overrides the provider's product client id.
- `clientSecretEnv` and `clientSecretFile` supply a client secret for providers that declare `clientSecretPost` or `clientSecretBasic`.
- `redirectUri` overrides the declared PKCE redirect URI when a BYO app requires an exact registered callback.
- `scopes` extends or replaces the provider's defaults.
- `domain` overrides the declared injection host for one-host deployments.
- `header` overrides the injected header name.

## Why providers carry their own client_id

A provider author registers an OAuth app on the upstream service once. The app's public `client_id` is baked into the provider's `.wasm` module via the auth manifest. With PKCE-public-client, no `client_secret` is needed, so the public `client_id` can ship in the binary unguarded; it identifies the provider as an OAuth client and that is all.

For vendors that demand a `client_secret` on the chosen flow, the provider author has two options:

1. Declare `defaultClientId: none` and require BYO (with the secret supplied via instance config). Google Workspace must do this for restricted scopes; Google's CASA audit makes a public project-owned app impractical for a community-distributed provider.
2. Run a small public bouncer service that holds the secret and brokers the OAuth dance (Slack's HTTPS-redirect workaround is the canonical example; the provider author runs `auth.example.com/slack/callback` and points the provider's `redirectUriTemplate` at it). This is a provider-author infrastructure decision, not a host concern.

The host neither runs nor knows about either path.

## Architecture

```text
                 ┌──────────────────────────────────────┐
                 │            HTTP callout              │
provider ─WIT─►  │           (fetch / fetch-blob)       │
                 └──────────────┬───────────────────────┘
                                │ url, headers, body
                                ▼
                        ┌───────────────┐
                        │   HttpStack   │  reqwest + capability check
                        └───┬───────┬───┘
                            │       │
                  headers_for_url   │ requires_auth_for_url
                            ▼       ▼
                     ┌──────────────────┐
                     │   AuthManager    │  trait object per mount
                     │   (generic)      │
                     │ ─────────────────│
                     │ • StaticToken    │ instance config + env/file
                     │ • OAuth2Pkce     │ provider metadata + store
                     └──────────────────┘
                            ▲
                            │ tokens, refresh callbacks
                            ▼
                     ┌──────────────────┐
                     │ CredentialStore  │  trait
                     │ ─────────────────│
                     │ • KeyringStore   │ macOS / Linux / Windows
                     │ • FileStore      │ ~/.omnifs/data (mode 600)
                     │ • MemoryStore    │ tests
                     └──────────────────┘

   ┌──────────────────────────────────────────────────────────┐
   │   Out-of-band login flow (driven by `omnifs auth login`) │
   ├──────────────────────────────────────────────────────────┤
   │  1. Load mount config and extracted auth manifest         │
   │  2. Pick the configured scheme (apply config overrides)  │
   │  3. Run generic OAuth 2.1 + PKCE engine using the        │
   │     provider-supplied metadata                            │
   │  4. Write token to CredentialStore                       │
   └──────────────────────────────────────────────────────────┘
```

The generic OAuth engine lives in `crates/omnifs-auth`. It takes OAuth scheme metadata from the auth manifest plus a `CredentialStore` handle and runs the flow. It does not import any provider's name and does not pattern-match on URLs.

## Configuration

Each mount's instance JSON gets an optional `auth` block. The provider's auth manifest is the source of defaults; the config is the override layer.

### Static token

```json
{
  "provider": "linear-provider.wasm",
  "mount": "linear",
  "auth": {
    "type": "static-token",
    "token_env": "LINEAR_API_KEY"
  }
}
```

The provider's auth manifest contains a `staticToken` scheme with the appropriate `headerName`, `valuePrefix`, and `injectDomains`. The host injects only on requests to those hosts.

### OAuth, provider-owned app

```json
{
  "provider": "linear-provider.wasm",
  "mount": "linear",
  "auth": {
    "type": "oauth",
    "scheme": "user",
    "account": "raul@example.com"
  }
}
```

`scheme` matches an `OauthScheme.key` from the provider's auth manifest. `account` is an opaque user-chosen handle that namespaces stored tokens (two mounts can hold tokens for two accounts of the same provider). Defaults come from the provider; the user supplies only what they want different.

### OAuth, BYO app

```json
{
  "provider": "google-workspace-provider.wasm",
  "mount": "google",
  "auth": {
    "type": "oauth",
    "scheme": "user",
    "account": "raul@example.com",
    "clientId": "1234567890-abcdef.apps.googleusercontent.com",
    "clientSecretEnv": "GOOGLE_OAUTH_CLIENT_SECRET",
    "redirectUri": "http://127.0.0.1:17890/callback",
    "scopes": [
      "https://www.googleapis.com/auth/gmail.readonly",
      "https://www.googleapis.com/auth/drive.readonly"
    ]
  }
}
```

### Non-HTTP credentials

```json
{ "auth": { "type": "kubeconfig", "path": "~/.kube/config", "context": "production" } }
{ "auth": { "type": "postgres-dsn", "dsn_env": "DATABASE_URL" } }
```

These are provider instance-config values, not HTTP auth schemes. A DSN has no `injectDomains` meaning; a file path is opaque provider configuration, usually paired with a WASI preopen or host-expanded config value. The host stores or redacts the config value but does not inject it into HTTP callouts, refresh it, or model it as an OAuth credential.

## PKCE auth-code flow (loopback redirect)

The flow runs out-of-band, driven by `omnifs auth login <mount>`. The daemon does not drive it on first FUSE access.

1. Load the mount's `auth` config. Resolve the `OauthScheme` from the extracted auth manifest by `scheme` key. Apply config overrides.
2. If `defaultClientId` is `none` and `auth.clientId` does not supply one, fail with a CLI message naming the BYO requirement. Otherwise construct the OAuth client with the resolved `authorizationEndpoint`, `tokenEndpoint`, and optional `revocationEndpoint`.
3. Generate a PKCE verifier and S256 challenge; generate a CSRF `state` value.
4. Bind a loopback HTTP listener on `127.0.0.1:0`. Read the port back, substitute into `redirectUriTemplate`, and register the redirect URI. Spawn a cancel token; the listener task awaits a child token.
5. Build the authorization URL with the requested scopes, the PKCE challenge, and any `extraAuthorizeParams` from the manifest.
6. Open the system browser. On failure, print the URL and instruct manual open; the listener stays up regardless.
7. Wait on the listener with a 5-minute deadline. On accept, parse the request line, verify `state` (constant-time compare), render a "you can close this tab" success page, cancel the token. State mismatch returns HTTP 400 and continues listening (browser prefetches sometimes hit the URL early).
8. Exchange the code with the PKCE verifier and any `extraTokenParams` from the manifest.
9. Compute `expires_at = now + expires_in - 60s` and assemble a `CredentialEntry`.
10. Write the entry to the store under key `(providerId, scheme, account)`.

The `pkceManualCode` shape replaces steps 4-7: no listener, no browser open, the redirect URI is the one the vendor enforces, the user pastes the code into a CLI prompt. Everything else is identical.

The `deviceCode` shape uses RFC 8628 polling: the user receives a verification URL and user code, the host polls the token endpoint until authorization completes, denial, or expiry.

## Token storage

The `CredentialStore` trait and storage layout are independent of which provider supplied the host-managed HTTP credential.

```rust
pub trait CredentialStore: Send + Sync {
    fn put(&self, key: &CredentialKey, entry: &CredentialEntry) -> Result<(), StoreError>;
    fn get(&self, key: &CredentialKey) -> Result<Option<CredentialEntry>, StoreError>;
    fn delete(&self, key: &CredentialKey) -> Result<(), StoreError>;
    fn list(&self) -> Result<Vec<CredentialKey>, StoreError>;
    fn supports_list(&self) -> bool { true }
    fn backend_label(&self) -> String;
}

pub struct CredentialKey {
    pub provider_id: String,  // stable provider identity, see open question 1
    pub scheme: String,       // OauthScheme.key or StaticTokenScheme.key
    pub account: String,      // user-chosen handle, opaque to the host
}

pub struct CredentialEntry {
    pub kind: CredentialKind,            // StaticToken | Oauth
    pub access_token: SecretString,
    pub refresh_token: Option<SecretString>,
    pub expires_at: Option<OffsetDateTime>,
    pub token_type: String,
    pub scopes: Vec<String>,
    pub stored_at: OffsetDateTime,
    pub last_validated: Option<OffsetDateTime>,
    pub upstream_identity: Option<String>,
    pub extras: BTreeMap<String, String>,
}
```

Three concrete implementations:

- `KeyringStore`: macOS Keychain, Linux libsecret, Windows DPAPI. Service name `omnifs`, account `{provider_id}:{scheme}:{account}`, value JSON-serialized `CredentialEntry`. Probed at startup; on failure the host falls back to the file store with a warning.
- `FileStore`: `~/.omnifs/data/credentials.json`, mode 600, atomic writes. Used when the keychain is unavailable (CI, containers, no session bus).
- `MemoryStore`: in-process map for tests.

Startup picks **one** backend (keychain or file). There is no dual-write store.

Encryption-at-rest for the file fallback is out of scope: the file is mode 600 inside the user's home directory, and the threat model treats user-account compromise as a separate problem.

## Refresh and retry

`OAuth2Pkce::headers_for_url(url)`:

1. Check if `url`'s host matches the scheme's `injectDomains`. If not, return no headers.
2. Read the store. Cache hit, not expired and not in the near-expiry window: return the header.
3. Cache hit, near-expiry or expired: trigger the refresh path. On success, return the new header; on permanent failure, clear store and return no headers (the request proceeds without auth and will 401, which the provider reports as `permission-denied`).

Mid-callout 401 retry in `HttpStack::send`:

1. Dispatch the request. On 200-2xx or non-auth error, return.
2. On 401 (or 403 with `WWW-Authenticate: Bearer error="invalid_token"`), call `auth.refresh_for_url(url)`. If refresh succeeds, rebuild headers and retry once. If the retry also 401s, surface the original response to the provider.

In-process refresh coalescing: a singleflight group keyed by `CredentialKey` collapses concurrent refreshes inside one host process. The Nth concurrent caller awaits the leader's future.

Cross-process correctness comes from a file lock on `~/.omnifs/data/credentials.lock`. The singleflight leader holds the exclusive lock while it (1) re-reads the durable store, (2) returns the freshly stored entry if another process already refreshed it, (3) calls the token endpoint with the latest refresh token, (4) writes the replacement entry, (5) updates the in-process slot.

The in-process current-token slot is an atomic swap container, so readers see either the pre-rotation or post-rotation `(access, refresh)` pair atomically, never torn. The provider's declared `refreshTokenRotates: true/false` is only a scheduling hint; the store re-read under the file lock is mandatory for all refreshes because a peer process may have rotated the token.

Refresh fires when `expires_at - now() < 60 s`.

## What providers must declare

A provider that needs OAuth must:

1. Register an OAuth app on the upstream service (or document a BYO requirement).
2. Embed an auth manifest containing at least one `oauth` scheme.
3. Set `injectDomains` to the minimum set of upstream API hosts. The host refuses injection elsewhere.
4. Set `defaultScopes` to the minimum scope set required for the provider's read paths. Mutation paths should be opt-in: providers declare an additional scheme with a write scope set, and the instance config picks.
5. Declare PKCE-public-client (`tokenEndpointAuth: none`) unless the vendor truly requires a secret. Most modern OAuth APIs support PKCE-public for native apps.
6. Pick the flow shape that actually works against the vendor: `pkceLoopback` by default, `pkceManualCode` if the vendor rejects loopback, `deviceCode` for OAuth apps without redirect support.

A provider that needs only static tokens embeds one `staticToken` scheme.

A provider that needs no HTTP credentials omits the auth manifest or embeds an empty scheme list.

A provider can declare multiple schemes. Linear, for example, declares both `staticToken` (for `lin_api_*` personal API keys) and `oauth` (for shared workspaces). The instance config picks which one to use via `auth.type`.

Each provider design under `docs/design/providers/` carries an "Authentication schemes" subsection that pins down its auth manifest entry.

## CLI surface

`omnifs auth` is a subcommand group. Every subcommand is provider-agnostic; the CLI reads metadata from the daemon and runs the generic engine.

| Command | Description |
|---|---|
| `omnifs auth login <mount>` | Run the configured scheme's flow. Opens the browser, runs the loopback listener (or prompts for a code), stores the token. Prints scopes and expiry. |
| `omnifs auth login <mount> --no-browser` | Print the URL to stdout instead of opening a browser. |
| `omnifs auth logout <mount>` | Delete the token from the store. With `--revoke`, call the vendor's `revocationEndpoint` if declared. |
| `omnifs auth status` | List all mounts with credentials. Show provider, scheme, account, scopes, expiry, last-refresh time, store backend. |
| `omnifs auth refresh <mount>` | Force a refresh. |
| `omnifs auth scopes <mount>` | Show currently-granted scopes vs the scheme's declared `defaultScopes`. |
| `omnifs auth import <mount> --token-env VAR` | Import a static token under the same store-key shape as OAuth. |
| `omnifs debug auth-manifest <mount>` | Print the extracted auth manifest for the mount's provider. Diagnostic / authoring aid. |

First-run experience: a mount config without a stored credential causes the first FUSE operation to fail with `permission-denied`; `omnifs status` flags the mount as "needs login" and points at `omnifs auth login <mount>`. The daemon does not pop a browser by itself.

## Manifest and runtime impact

This design does **not** modify the WIT for auth. The provider-auth contract is the `auth` block inside the embedded `omnifs.provider-metadata.v1` section. Providers that omit auth metadata are treated as needing no HTTP credentials.

The host runtime:

1. `AuthManager` is a trait dispatched per URL. The static-token path is one implementation; OAuth is another. A wrapping type holds `Vec<Box<dyn AuthStrategy>>`.
2. At mount load, the host reads provider metadata from wasm, derives the auth manifest, picks the strategy implied by the instance config's `auth.type` and (for OAuth) `auth.scheme`, and constructs the strategy with merged metadata.
3. `HttpStack::send` runs the 401-retry path.

A future WIT extension may add a per-callout `auth-context` arm so providers can request specific scopes or named accounts per call (multi-account Google Workspace). Out of scope here.

## Failure modes

All diagnostic surfaces (`omnifs auth status`, `omnifs auth scopes`, `omnifs debug auth-manifest`) read from the auth manifest and the store, not from any host-internal table. Failure modes like "scope drift" surface clearly: the diff between the scheme's declared `defaultScopes` and the stored entry's `scopes` is visible immediately.

| Failure | Symptom | Recovery |
|---|---|---|
| Refresh token revoked or expired beyond reissue | Refresh endpoint returns 400 `invalid_grant`. | Strategy clears the store entry, returns no headers. Next callout 401s; provider reports `permission-denied`. User runs `omnifs auth login <mount>`. |
| Provider declares `defaultClientId: none` and config omits `auth.clientId` | `omnifs auth login` fails with a clear message naming the BYO requirement. | User supplies `auth.clientId` and re-runs. |
| Network down during refresh | Refresh exchange fails with transient error. | Strategy returns the cached token (if still within validity), logs a warning. Otherwise the callout 401s with a `network` underlying cause. |
| Browser does not open | `webbrowser::open` returns error or no DISPLAY. | CLI falls back to printing the URL. Listener still runs; user opens URL elsewhere and ports the redirect (or uses the manual-code shape if the scheme declares it). |
| Loopback port blocked by firewall | Listener bind succeeds but callback never arrives. | Manual-code fallback if the scheme allows; otherwise document the firewall as a known limitation. |
| Vendor changes auth URL or token URL | Provider's declared endpoints become stale. | Provider author ships a new `.wasm` with corrected metadata. The host requires no release. |
| Scope drift (provider needs new scope after a release) | Vendor returns `permission-denied` from a specific endpoint. | `omnifs auth scopes <mount>` shows the gap between declared and granted; user reruns `omnifs auth login` to consent again. |
| Two mounts share an OAuth account | Both try to refresh simultaneously. | In-process singleflight avoids duplicate local work; the cross-process file lock serializes the durable refresh. Identical `(providerId, scheme, account)` shares a token. |
| Keychain unavailable | Read returns `NotFound` or `Backend::Unavailable`. | Probe at startup; on failure, fall back to file store with a warning. |
| Provider auth manifest declares an unsupported flow shape | Strategy construction fails. | CLI surfaces "this mount needs host version ≥ X to authenticate". |

## Open questions

1. **Provider identity for store keys.** Using a content hash of the `.wasm` keeps credentials tied to a specific provider build, which is correct for security but breaks credentials across provider upgrades. Alternative: a stable provider-author identifier embedded in the auth manifest (`providerId = "linear-provider/v1"`). Likely worth taking; the upgrade ergonomics outweigh the security strictness.

2. **GitHub App installation flow.** GitHub Apps mint short-lived (1 h) installation tokens via JWT-signed `/app/installations/<id>/access_tokens`. That is a different protocol; it does not fit `oauth`. The auth manifest could grow a `githubApp(...)` arm, or the GitHub provider could implement that token minting itself via a callout and pass the token through a `staticToken` scheme that the provider keeps refreshing internally. Probably the latter: keep the auth manifest small. Generalize only if a second provider needs the same shape.

3. **Per-callout auth context.** A future WIT extension lets a single callout request a specific scheme or account ("use the read-only token for this list, the read-write token for this open"). Defer until a mutation-capable provider lands.

4. **Token revocation correctness.** Not every vendor honors RFC 7009. Provider-supplied `revocationEndpoint` is best-effort; `omnifs auth logout` succeeds locally even if revocation fails server-side.

5. **Daemon-driven login.** Today the CLI process runs the OAuth flow and tells the daemon to reload. The alternative (daemon runs the flow over a local RPC, CLI relays) is cleaner for in-flight retries. Defer.

6. **Provider-supplied UI strings.** The provider declares `displayName`; should it also declare a help URL the CLI prints during login? Cheap manifest addition; consider when the next provider lands.

## References

- [RFC 6749](https://datatracker.ietf.org/doc/html/rfc6749) OAuth 2.0
- [RFC 7636](https://datatracker.ietf.org/doc/html/rfc7636) PKCE
- [RFC 8252](https://datatracker.ietf.org/doc/html/rfc8252) OAuth 2.0 for Native Apps (BCP 212)
- [OAuth 2.1 draft](https://datatracker.ietf.org/doc/draft-ietf-oauth-v2-1/)
- [RFC 8628](https://datatracker.ietf.org/doc/html/rfc8628) Device Authorization Grant
- [RFC 7009](https://datatracker.ietf.org/doc/html/rfc7009) Token Revocation
- [MCP authorization spec](https://modelcontextprotocol.io/specification/2025-06-18/basic/authorization)
