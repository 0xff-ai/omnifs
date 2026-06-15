---
title: Add auth to a provider
description: Fetch from an authenticated API using a host-injected credential, without the provider ever holding the token.
---

# Add auth to a provider

Goal: fetch from an authenticated API with a host-injected credential, without the provider ever holding the token.

Declare the auth scheme and domain in `omnifs.provider.json`. A static-token scheme names the header and the domains it may be injected on:

```json
{
  "id": "example",
  "auth": {
    "schemes": [
      { "type": "static-token", "key": "api-key",
        "headerName": "Authorization", "valuePrefix": "Bearer ",
        "injectDomains": ["api.example.com"] }
    ]
  },
  "capabilities": { "domains": ["api.example.com"] }
}
```

Declare the endpoint and call it from a handler:

```rust
let item = cx.endpoint::<ExampleApi>()
    .get("/v1/items/42")
    .json::<Item>()
    .await?;
```

Then import a token and read:

1. `omnifs auth import example --token-env EXAMPLE_TOKEN`
2. `omnifs up`
3. `cat /omnifs/example/items/42/name`

## Result

The handler issued a plain GET. The host attached `Authorization: Bearer ...` because the manifest authorized injection on `api.example.com`, ran the request, and returned the bytes. The provider saw only the response. It never saw the credential, and it could not have reached any other domain.
