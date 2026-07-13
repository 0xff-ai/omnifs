# omnifs-provider-web

[omnifs](https://github.com/0xff-ai/omnifs) provider that projects allowed HTTPS pages as readable Markdown files and raw response bytes.

## Mount layout

```text
/web/https/{host}/@root
/web/https/{host}/{percent-encoded-path}
/web/raw/https/{host}/@root
/web/raw/https/{host}/{percent-encoded-path}
```

Each URL resource is one filename beneath its configured host directory. `@root` is the explicit site-root leaf and fetches `https://{host}/`. A multi-segment URL path percent-encodes each slash in that filename, so `docs%2Fguide` fetches `https://{host}/docs/guide`.

`/web/https/{host}/{resource}` fetches the URL, extracts the readable article content in the provider WASM guest with `dom_smoothie`, and returns Markdown. `/web/raw/https/{host}/{resource}` fetches the same URL and returns the response bytes verbatim.

Query strings stay in the filename, so `/web/raw/https/news.ycombinator.com/item?id=48884815` fetches exactly `https://news.ycombinator.com/item?id=48884815`. Fragments and traversal segments are rejected.

## Capabilities

The provider declares a dynamic domain need. Each mount lists allowed hostnames in `config.domains`, and the host resolves the mount's dynamic domain grant from that list. Wildcard domains are intentionally not supported.

## Limitations

Only HTTPS URLs are projected. The provider does not enumerate upstream pages beneath a host; host directories are enumerable from the mount's configured `domains` list, while page listings remain open and lookup-driven.

## Install

This is a wasm component. Build with:

```bash
cargo build --target wasm32-wasip2 --release -p omnifs-provider-web
```

Release CLI binaries embed this provider and unpack it into `OMNIFS_HOME/providers`.

## License

Dual licensed under MIT or Apache-2.0 at your option.
