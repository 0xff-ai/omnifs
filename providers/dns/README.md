# omnifs-provider-dns

[omnifs](https://github.com/0xff-ai/omnifs) provider that resolves DNS queries over DNS-over-HTTPS and projects each record type as a file. Default resolver is Cloudflare; `/dns/@{resolver}/...` scopes a query to a specific resolver, and `/dns/reverse/{ip}` does PTR lookups.

## Mount layout

```
/dns/{name}/
  A AAAA MX NS TXT CNAME SOA SRV
  all raw

/dns/@cloudflare/{name}/...
/dns/@google/{name}/...

/dns/reverse/{ip}/PTR
```

`all` projects every record type for the name as a single file; `raw` is the wire-format binary response.

## Capabilities

HTTPS to `cloudflare-dns.com` and `dns.google` (resolver list is configurable). 32 MiB memory limit. Read-only.

## Install

This is a wasm component. Build with:

```bash
cargo build --target wasm32-wasip2 --release -p omnifs-provider-dns
```

Release CLI binaries embed this provider and unpack it into `OMNIFS_HOME/providers`. Run `omnifs mount add dns` to create its mount spec.

## Status

Pre-1.0. Resolver scoping and reverse-lookup paths may evolve.

## License

Dual licensed under MIT or Apache-2.0 at your option.
