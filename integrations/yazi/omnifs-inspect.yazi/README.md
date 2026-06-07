# omnifs-inspect.yazi

A [Yazi](https://yazi-rs.github.io) previewer that renders the **omnifs
inspector stream** as live, correlated, per-operation traces using Yazi's
native widgets.

It is a pure consumer of the existing inspect protocol. Each refresh shells out
to `omnifs inspect --dump` for a one-shot snapshot of the daemon's history ring
— the same records served on the `:7878` socket
(`omnifs-inspector::InspectorRecord`). There are **no host caching or schema
changes**. Traces only; there is deliberately no cache viewer.

## What it shows

Records are grouped by FUSE `trace_id` into one block per operation, refreshed
in place while the file stays hovered:

```
● trace 42  lookup  /github/torvalds  1.2ms  ok
    github.lookup_child  /torvalds  900µs  ok
    callout fetch  GET api.github.com/users/torvalds  720µs  ok
    cache browse_miss  /torvalds
```

The FUSE op is the bold header (with its total elapsed + outcome); provider,
callout, cache, clone, and subtree events are indented beneath it. Outcomes are
green for `ok`, red otherwise. Scroll with the normal preview scroll keys.

## Install

The supported path is the CLI:

```sh
omnifs features add yazi      # installs this plugin, registers the previewer,
                              # and drops a sentinel file to hover
```

`omnifs features add yazi` prints the sentinel path; open it in Yazi
(`yazi <path>`) and the live trace view appears. Because the inspector socket is
published to the host by `omnifs up` / `omnifs dev`, this works in **host-side
Yazi** — the FUSE mount is not required, only the `omnifs` CLI on `PATH`.

To install by hand instead:

```sh
cp -r omnifs-inspect.yazi ~/.config/yazi/plugins/
```

and add to `~/.config/yazi/yazi.toml`:

```toml
[plugin]
prepend_previewers = [
  { name = "*.omnifs-inspect", run = "omnifs-inspect" },
]
```

then hover any file ending in `.omnifs-inspect`.

## Notes / limitations

- The previewer ignores the hovered file's contents; the file is only a match
  trigger. Any `*.omnifs-inspect` file works.
- JSONL is parsed with anchored Lua patterns rather than a full JSON decoder
  (no `jq` dependency). Values containing escaped quotes may be truncated in the
  display — acceptable for these short labels/paths.
- Caps at the 60 most recent traces per refresh for responsiveness.

Licensed under the repository's MIT OR Apache-2.0 terms.
