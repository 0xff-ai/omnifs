# omnifs-inspect.yazi

A proof-of-concept [Yazi](https://yazi-rs.github.io) previewer that renders the
**omnifs inspector stream** as correlated, per-operation traces using Yazi's
native widgets.

It is a pure consumer of the existing inspect protocol — the same JSONL records
the daemon serves on its `:7878` socket (`omnifs-inspector::InspectorRecord`).
There are **no host or Rust changes**: the plugin parses recorded inspector
JSONL and draws it. Traces only; there is deliberately no cache viewer.

## What it shows

Records are grouped by FUSE `trace_id` into one block per operation, e.g.:

```
● trace 42  lookup  /github/torvalds  1.2ms  ok
    github.lookup_child  /torvalds  900µs  ok
    callout fetch  GET api.github.com/users/torvalds  720µs  ok
    cache browse_miss  /torvalds
```

The FUSE op is the bold header (with its total elapsed + outcome); provider,
callout, cache, clone, and subtree events are indented beneath it. Outcomes are
green for `ok`, red otherwise.

## Try it (inside the omnifs container)

Yazi must run where the omnifs FUSE mount and inspector socket live — i.e.
**inside the container** (`omnifs shell`). `jq` and `yazi` must be on `PATH`.

```sh
# 1. Tee the live inspector stream to a file (history snapshot + live tail):
bash -c 'exec 3<>/dev/tcp/127.0.0.1/7878; cat <&3 > /tmp/omnifs-inspect.jsonl &'

# 2. Generate some traffic so there is something to trace:
ls /github/torvalds; cat /dns/cloudflare.com/A

# 3. Open Yazi and hover the file:
yazi /tmp        # move the cursor onto omnifs-inspect.jsonl
```

Scroll the trace view with the normal preview scroll keys.

## Install

```sh
cp -r omnifs-inspect.yazi ~/.config/yazi/plugins/
```

Register the previewer in `~/.config/yazi/yazi.toml`:

```toml
[plugin]
prepend_previewers = [
  { name = "*omnifs-inspect*.jsonl", run = "omnifs-inspect" },
]
```

## Scope / limitations

- **PoC.** File-sourced (hover a recorded stream); a live socket-backed mode
  (a functional plugin that follows `:7878` directly) is a natural next step.
- Depends on `jq` for JSON→TSV projection, matching Yazi's built-in `json`
  previewer. A vendored Lua decoder could remove that dependency.
- Caps at the 60 most recent traces per preview for responsiveness.

Licensed under the repository's MIT OR Apache-2.0 terms.
