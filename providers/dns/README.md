# omnifs-provider-dns

[omnifs](https://github.com/0xff-ai/omnifs) provider that resolves DNS queries over DNS-over-HTTPS and projects each record type as a file. Default resolver is Cloudflare; `/dns/@{resolver}/...` scopes a query to a specific resolver, and `/dns/_reverse/{ip}` does PTR lookups.

## Mount layout

```
/dns/{name}/
  A AAAA MX NS TXT CNAME SOA SRV
  _all _raw

/dns/@cloudflare/{name}/...
/dns/@google/{name}/...

/dns/_reverse/{ip}/PTR
```

`_all` projects every record type for the name as a single file; `_raw` is the wire-format binary response.

## Capabilities

HTTPS to `cloudflare-dns.com` and `dns.google` (resolver list is configurable). 32 MiB memory limit. Read-only.

## Install

This is a wasm component. Build with:

```bash
cargo build --target wasm32-wasip2 --release -p omnifs-provider-dns
```

The resulting `omnifs_provider_dns.wasm` is also attached to each [GitHub Release](https://github.com/0xff-ai/omnifs/releases). Configure in your omnifs mount config under the `dns` key.

## Status

Pre-1.0. Resolver scoping and reverse-lookup paths may evolve.

## License

Dual licensed under MIT or Apache-2.0 at your option.
