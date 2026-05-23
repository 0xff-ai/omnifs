# omnifs-auth

`omnifs-auth` is the host-side OAuth adapter for Omnifs. It does not implement
OAuth itself; protocol construction, PKCE, device-code polling, token refresh,
revocation, request signing, and response parsing are delegated to the `oauth2`
crate.

This crate owns the Omnifs-specific boundary around that protocol:

- building `oauth2::basic::BasicClient` values from provider auth manifests
- applying mount-level overrides such as scopes, redirect URI, client ID, and
  client secret
- running the user interaction around loopback, manual-code, and device-code
  login flows
- converting successful token responses into `omnifs_creds::CredentialEntry`
- mapping oauth2 transport and endpoint errors into `AuthError`

The HTTP client is a plain `reqwest::Client` configured with redirects disabled,
as recommended by `oauth2`. Provider-specific behavior belongs in manifest
configuration or explicit Omnifs policy, not in a second OAuth transport layer.
