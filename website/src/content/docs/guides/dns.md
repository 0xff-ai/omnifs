---
title: Querying DNS
description: Read DNS records as files under /dns — record types, specific resolvers, reverse lookups, and raw responses.
---

The DNS provider mounts under `/dns`. Query DNS by reading files — no `dig`, no
`nslookup`. Each record type is a file under the domain's directory.

```bash
cat /dns/example.com/A
cat /dns/google.com/MX
```

## Path map

| Path | Content |
|------|---------|
| `/dns/<domain>/A` | IPv4 addresses |
| `/dns/<domain>/AAAA` | IPv6 addresses |
| `/dns/<domain>/MX` | Mail exchangers |
| `/dns/<domain>/TXT` | TXT records |
| `/dns/<domain>/NS` | Name servers |
| `/dns/<domain>/CNAME` | Canonical name |
| `/dns/<domain>/SOA` | Start of authority |
| `/dns/<domain>/all` | All record types at once |
| `/dns/<domain>/raw` | Raw resolver response |
| `/dns/@<resolver>/<domain>/<type>` | Query via a specific resolver |
| `/dns/reverse/<ip>` | Reverse DNS (PTR) |
| `/dns/<ip>` | Reverse DNS shorthand |
| `/dns/resolvers` | Configured resolvers |

## Record types

Read any record type by name under the domain directory.

```bash
cat /dns/example.com/A
cat /dns/example.com/AAAA
cat /dns/example.com/TXT
cat /dns/example.com/NS
```

To pull everything at once, read `all`. For the unparsed resolver answer, read
`raw`.

```bash
cat /dns/example.com/all
cat /dns/example.com/raw
```

## Using a specific resolver

Prefix the domain with `@<resolver>` to query through a particular resolver
instead of the default.

```bash
cat /dns/@1.1.1.1/example.com/A
cat /dns/@8.8.8.8/example.com/MX
```

List the resolvers omnifs is configured to use:

```bash
cat /dns/resolvers
```

## Reverse lookups

Reverse DNS (PTR) is available two ways: the explicit `reverse/<ip>` path or the
`<ip>` shorthand at the mount root.

```bash
cat /dns/reverse/8.8.8.8
cat /dns/8.8.8.8
```

:::tip
Checking a mail or verification misconfiguration is a single read. Compare the
default resolver against a public one to spot stale or split-horizon answers:

```bash
diff <(cat /dns/mycompany.com/MX) <(cat /dns/@1.1.1.1/mycompany.com/MX)
cat /dns/mycompany.com/TXT
```
:::
