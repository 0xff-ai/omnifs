# omnifs-provider-web

[omnifs](https://github.com/0xff-ai/omnifs) provider that projects allowed HTTPS pages as readable Markdown files and raw response bytes.

## Mount layout

```text
/web/https/{host}/{path}
/web/raw/https/{host}/{path}
```

`/web/https/{host}/{path}` fetches `https://{host}/{path}`, extracts the readable article content in the provider WASM guest with `dom_smoothie`, and returns Markdown.

`/web/raw/https/{host}/{path}` fetches the same URL and returns the response bytes verbatim.

The empty rest path maps to the site root URL, so `/web/https/example.com` fetches `https://example.com/`.

## Capabilities

The provider declares a dynamic domain need. Each mount lists allowed hostnames in `config.domains`, and the host resolves the mount's dynamic domain grant from that list. Wildcard domains are intentionally not supported.

## Limitations

Only HTTPS URLs are projected. Query strings and fragments are not supported in v1.

## Install

This is a wasm component. Build with:

```bash
cargo build --target wasm32-wasip2 --release -p omnifs-provider-web
```

Release CLI binaries embed this provider and unpack it into `OMNIFS_HOME/providers`.

## License

Dual licensed under MIT or Apache-2.0 at your option.
