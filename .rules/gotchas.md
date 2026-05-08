# Gotchas

**Read when:** writing or reviewing provider code, touching the FUSE layer,
sizing/streaming a projected file, working with hashmaps inside a provider,
or running into a "this should work but doesn't" surprise. Skim before
writing your first new provider handler.

**Update when:** discovering a new code-level surprise that an agent could
plausibly regress without a written-down warning. Add a new entry rather
than rewriting an old one — these are footguns, and the framing usually
matters.

---

## Projected file sizes use a 256 MiB placeholder by default

`Projection::file(name)` and SDK-emitted exact-shape file entries fall back
to `placeholder_size()` (256 MiB) when the real length isn't known until
`read`. The kernel caps `read` at the reported size and treats short reads
as EOF, so a too-small placeholder truncates real payloads; 256 MiB is the
current upper bound. The host updates the inode size to the actual length
on the first successful read (`inode.rs`'s `get_or_alloc_ino` and_modify
path), so subsequent stats report the real size.

If your provider knows the size cheaply (Content-Length, API metadata,
fully-materialized payload), use `Projection::file_with_size(name, FileStat
{ size })` or `Projection::file_with_content(name, bytes)` so `ls -l` and
`du` don't show the inflated placeholder before first read. See
`docs/design/projected-file-sizes.md` for the full rationale and the
planned `direct_io` redesign.

## Project sibling files on every read where you already know them

When a read route materializes a payload that contains fields for sibling
files (e.g. an issue's `title`, `body`, `state`), return them in
`FileContent::with_sibling_files(..)` so the host caches them alongside the
primary file. A later stat or read of a sibling avoids a round trip. The
same applies to lookup routes: use `Lookup::with_sibling_files(..)` whenever
the payload you fetched already carries the sibling content.

For content that isn't a direct sibling of the looked-up target (say, nested
children of a listed directory), use `Projection::preload` /
`preload_many`; those land on the terminal's preload field. See
`.rules/provider-sdk.md` and `.rules/caching.md`.

## `hashbrown::HashMap` vs `std::collections::HashMap` in providers

Use `hashbrown` for provider-internal maps. It keeps provider internals
predictable across WASI targets.

## Provider tests can't execute on `wasm32-wasip2` directly in Cargo's test harness

Always use `--no-run` for target-specific compilation checks. Tests that
need to actually run should use `#[cfg(test)]` with host-target-compatible
code only.

## `[package.metadata.component]` sections in provider Cargo.toml are vestigial

They are leftovers from `cargo component build` and unused by the current
build pipeline. Kept for documentation of the WIT world mapping; don't
treat them as authoritative.

## Container logs vs runtime logs

`docker compose logs omnifs` shows stdout/stderr from the entrypoint.
Runtime FUSE traces go to `/tmp/omnifs.log` inside the container. Check
both when debugging — they show different things. (Same point appears in
`.rules/debugging.md`; this is the in-context reminder when you're
mid-edit.)
