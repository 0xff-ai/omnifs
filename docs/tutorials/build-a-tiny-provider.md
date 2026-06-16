---
title: Build a tiny provider
description: Register one route and read it in a shell to see the minimal provider shape.
---

# Build a tiny provider

Goal: register one route and read it in a shell.

Write the provider. A minimal path-oriented provider is one file that registers one file route:

```rust
#[omnifs_sdk::provider(metadata = "omnifs.provider.json")]
impl HelloProvider {
    type State = ();

    fn start(_config: NoConfig, r: &mut Router<()>) -> Result<()> {
        r.file("/greeting").handler(|_cx: Cx<()>| async move {
            Ok(FileProjection::inline(b"hello from a provider\n".to_vec())
                .immutable()
                .build())
        })?;
        Ok(())
    }
}
```

Provide a minimal `omnifs.provider.json` next to it declaring the provider id and no capabilities. Then:

1. `just providers-build`
2. Mount it and bring the runtime up.
3. `cat /omnifs/hello/greeting`

## Result

`cat` prints your bytes. You wrote no I/O, no caching, and no kernel code. You declared one route that returns inline bytes with an immutable stability, and the host did the rest. From here, adding a capture (`/greeting/{name}`) makes the route dynamic, and reaching upstream means awaiting a callout. Both are covered in the providers plane.
