# OAuth authentication

omnifs ships product OAuth client ids in each provider's `providers/<name>/omnifs.provider.json` manifest, under `auth`. The CLI embeds those provider manifests for first-run setup, and the provider build embeds the same metadata in the wasm `omnifs.provider-metadata.v1` section. These ids are public OAuth application identifiers, not secrets. The sensitive values are user access tokens, refresh tokens, and any client secrets required by a provider that cannot use a public-client flow.

GitHub uses OAuth device authorization with client id `Ov23licogxMDzS47s9sF` and no default scopes. GitHub treats no-scope OAuth tokens as read-only for public information; do not add `repo` unless you explicitly accept GitHub OAuth's broad private-repository read/write grant.

Linear uses authorization-code + PKCE with client id `4dc7b7c05f651306a318de6f9f963b40`, redirect shape `http://127.0.0.1:{port}/callback`, and default scope `read`.

## Configure mounts

`omnifs init` owns mount generation. It discovers provider defaults from the CLI's built-in catalog, with host-side wasm metadata allowed to override or extend that catalog for development and third-party providers. It writes a thin mount config and immediately runs the provider's default OAuth flow.

```bash
omnifs init github
omnifs init linear
omnifs status
omnifs up
```

The generated mount configs only record the selected provider, mount name, and auth scheme. Capabilities and default provider config are inherited from the built-in catalog on the host and from provider wasm metadata inside the runtime image. Auth scheme definitions come from the provider manifest in both places.

```json
{
  "provider": "omnifs_provider_github.wasm",
  "mount": "github",
  "auth": {
    "type": "oauth",
    "scheme": "device"
  }
}
```

```json
{
  "provider": "omnifs_provider_linear.wasm",
  "mount": "linear",
  "auth": {
    "type": "oauth",
    "scheme": "oauth"
  }
}
```

`auth.clientId`, `auth.redirectUri`, and `auth.scopes` remain available as explicit overrides for development or self-hosted OAuth apps, but they are not required for the bundled GitHub and Linear flows.

## Login

`omnifs init github` prints the GitHub device URL and user code. `omnifs init linear` opens a loopback PKCE login in the browser. `omnifs auth login <mount>` remains available for re-authentication and repair after the mount exists.

Use `--no-browser` when you need the CLI to print the authorization URL instead of launching a browser. Use repeated `--scope` flags to request non-default OAuth scopes:

```bash
omnifs init github --scope repo
omnifs auth login github --scope repo
```

GitHub's default no-scope flow is intentionally public-read only. Request `repo` only when you want private repository access and accept GitHub OAuth's broad private-repository grant.

`omnifs status` is the main readiness view after init. It shows generated mounts, provider availability, auth readiness, credential expiry when available, and the recovery command for missing credentials.
