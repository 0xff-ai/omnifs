# Linear provider

The Linear provider projects teams and issues from Linear's GraphQL API at `https://api.linear.app/graphql`.

## Authentication schemes

The provider declares two host-managed HTTP auth schemes in `auth` inside `providers/linear/omnifs.provider.json`; the provider build embeds that metadata as the wasm `omnifs.provider-metadata.v1` section.

`staticToken` with key `pat` covers Linear personal access tokens and API keys. The host injects the raw token into the `Authorization` header for `api.linear.app`; Linear PATs are not prefixed with `Bearer `.

`oauth` with key `oauth` declares Linear's authorization-code + PKCE flow:

- Authorization endpoint: `https://linear.app/oauth/authorize`
- Token endpoint: `https://api.linear.app/oauth/token`
- Redirect shape: `http://127.0.0.1:{port}/callback`
- Default scope: `read`
- Injected API header: `Authorization: Bearer <access token>`

Linear documents PKCE as supported for OAuth applications. Its token endpoint treats `client_secret` as optional for PKCE authorization-code exchange, and refresh for PKCE-generated tokens can use `client_id` without `client_secret`.

The provider bakes product OAuth client id `4dc7b7c05f651306a318de6f9f963b40` into its auth manifest. Live OAuth login therefore only needs the mount config to select the OAuth scheme:

```json
{
    "auth": {
      "type": "oauth",
      "scheme": "oauth"
    }
}
```

Static-token auth remains the default supported container workflow.

## Validation

The provider has a unit test that decodes the macro-emitted auth manifest bytes and asserts both schemes, including the product client id. The generic host OAuth path is tested with fake OAuth and HTTPS API servers, including the BYO `clientId` override. No live Linear OAuth flow is part of CI.

References:

- https://linear.app/developers/oauth-2-0-authentication
