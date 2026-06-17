# Protocol and provider model

This note maps the host-facing filesystem protocols to the omnifs provider
contract. The goal is to keep FUSE and NFSv4 mechanics in the host while
letting providers describe domain facts, byte sources, and useful work hints.

The provider contract should stay protocol-neutral. Providers should not know
whether a request came from `cd`, `ls`, `cat`, a FUSE callback, or an NFSv4
compound. They should answer questions in omnifs terms: lookup a child, list a
directory, read a file, open a ranged byte source, read a chunk, close a byte
source, and return facts or hints such as `FileAttrs`, sibling projections,
preloads, stability, and version tokens.

## FUSE mapping

FUSE exposes callbacks. The host turns those callbacks into provider calls only
when the inode, directory snapshot, attrs, or file bytes are not already known.

| FUSE operation | Description | Common bash triggers | Provider correspondence |
| --- | --- | --- | --- |
| `lookup(parent, name)` | Resolve one path component and return inode, type, and attrs. | `cd /dns/google.com`, `ls /dns/google.com`, `stat path`, `cat path` before open. | On miss, call `lookup_child(parent_path, name)`. This is the right point for synchronous path validation. |
| `getattr(ino)` | Return mode, type, size, timestamps, and related attrs. | `stat`, `ls -l`, `test -s`, shell completion, many tools before reads. | No provider call today. Uses inode/cache attrs learned from lookup, listing, open, or read. |
| `opendir(ino)` | Open a directory handle and build a directory snapshot. | `ls dir`, `find dir`, `grep -r dir`. | On miss, call `list_children(path)`. This is where provider directory projection happens. |
| `readdir(fh, offset)` | Stream directory entries from the opened snapshot. | `ls`, `find`, shell globbing. | No provider call today. Serves the snapshot built by `opendir`. |
| `releasedir(fh)` | Close a directory handle. | End of `ls`, `find`, shell globbing. | No provider call. Drops host directory snapshot. |
| `open(ino)` | Open a file handle. | `cat file`, `head file`, `tail file`, `wc file`, editors. | For `ByteSource::Deferred(ReadMode::Ranged)`, call `open_file(path)`. For non-exact `ByteSource::Deferred(ReadMode::Full)`, materialize through `read_file(path)` before the first read. |
| `read(fh, offset, size)` | Read bytes from a file. | `cat`, `head`, `tail -c`, `wc -c`, `grep`, `cp`, `tar`. | Full mode miss calls `read_file(path)`. Ranged mode calls `read_chunk(handle, offset, length)`. |
| `release(fh)` | Close a file handle. | End of `cat`, `head`, `cp`, editor close. | For ranged handles, call `close_file(handle)`. Otherwise drop per-handle host cache. |
| `readlink(ino)` | Read symlink target. | `readlink`, `ls -l` on symlink. | Backing filesystem only today. There is no provider symlink contract. |
| `flush(fh)` | Flush close-time state. | Often appears after reads depending on kernel and tool behavior. | Not implemented today. No provider call. |
| `access(path, mode)` | Check access bits. | `test -r`, shell checks, some `ls` paths. | Host-derived from read-only attrs and mode. No provider call. |
| `statfs` | Return filesystem capacity and limits. | `df`, some traversal tools. | Synthetic host data if added. No provider call. |
| Write and mutation operations | Modify filesystem state. | `echo > file`, `touch`, `mkdir`, `rm`, `mv`, `chmod`. | No provider correspondence yet. Return read-only errors until mutation protocol exists. |

For DNS, `cd /dns/google.com` is lookup and maybe getattr. It is not a
content read. `ls /dns/google.com` is opendir and readdir. `cat
/dns/google.com/A` is open, read, and release.

## NFSv4 mapping

NFSv4 uses `COMPOUND` requests over filehandles instead of callback-style
operations. The same provider contract still applies.

| FUSE operation | NFSv4 primitive or compound shape | Provider correspondence |
| --- | --- | --- |
| `lookup(parent, name)` | `PUTFH parent; LOOKUP name; GETFH; GETATTR`. | On miss, call `lookup_child(parent_path, name)`. This remains the right point for synchronous path validation. |
| `getattr(ino)` | `PUTFH fh; GETATTR attr-bitmap`. | No provider call if attrs are cached. Uses projected attrs. May force lookup or listing first if filehandle state is cold. |
| `opendir` | No exact primitive. | Server prepares directory state internally before `READDIR` if needed. |
| `readdir` | `PUTFH dirfh; READDIR cookie verifier attr-request`. | On miss, call `list_children(path)`. NFSv4 can return requested per-entry attrs inline, avoiding some FUSE-style per-child getattr traffic. |
| `releasedir` | No exact primitive. | No provider call. Server can drop snapshot state after request, session expiry, or cache eviction. |
| `open` | `PUTFH fh; OPEN; GETFH; GETATTR`, though read-only clients may skip explicit open. | For ranged files, call `open_file(path)` when a provider handle or snapshot is semantically required. Full reads can defer provider work until `READ`. |
| `read` | `PUTFH fh; READ offset count`. | Full mode miss calls `read_file(path)`. Ranged mode calls `read_chunk(handle, offset, length)`. |
| `release` | `PUTFH fh; CLOSE stateid`. | For ranged handles, call `close_file(handle)`. Otherwise drop per-open host state. |
| `readlink` | `PUTFH fh; READLINK`. | Only if omnifs adds provider symlink support. |
| `access` | `PUTFH fh; ACCESS mask`. | Host-derived from read-only attrs and mode. No provider call. |
| `statfs` | `PUTFH rootfh; GETATTR filesystem attrs`. | Synthetic host data. No provider call. |
| Write and mutation operations | `CREATE`, `REMOVE`, `RENAME`, `SETATTR`, `WRITE`, `COMMIT`, and related compounds. | No provider correspondence yet. Return read-only errors until mutation protocol exists. |

NFSv4 brings a few extra host responsibilities:

- Filehandles should resolve to stable omnifs identity: provider id, path or
  file identity, and enough generation/version data to reject stale handles.
- `OPEN`, `READ`, and `CLOSE` stateids map naturally to dynamic ranged
  snapshots and live handles.
- `READDIR` can include per-entry attrs. The NFS frontend should use projected
  attrs directly instead of forcing an extra getattr path when clients ask for
  them.
- NFS clients cache using attrs such as `size`, `change`, `ctime`, and `mtime`.
  `Dynamic` and `Live` files need disciplined change values and should not
  receive delegations that imply stronger stability than omnifs can honor.
- NFS still needs a size attribute. The file-attributes contract maps directly:
  `Size::Exact(n)` reports `n`, `Size::NonZero` reports `1`, and
  `Size::Unknown` reports `1` before materialization as a compatibility
  sentinel, then reports the learned exact size after open materialization or
  another complete read observation.

## Provider boundary

Providers should not implement FUSE or NFSv4 operations directly. Doing so
would turn the SDK into a leaky VFS API and push protocol mechanics into every
provider: inode and filehandle identity, NFS cookies, stateids, direct I/O,
kernel caches, attr invalidation, access checks, and client-specific behavior.

The host should own protocol mechanics. Providers should own domain facts and
byte production.

| Protocol need | Provider-level concept |
| --- | --- |
| Resolve a child | `lookup_child(parent_path, name)` |
| List a directory | `list_children(path)` |
| Return attrs | `FileAttrs` on entries, projected files, preloads, open results, and read results |
| Open a semantic byte source | `open_file(path)` only when a ranged provider handle is needed |
| Read bytes | `read_file(path)` or `read_chunk(handle, offset, length)` |
| Close a provider byte source | `close_file(handle)` |
| Cache and invalidation | Provider facts and hints plus host policy |

The right extension points are protocol-neutral provider concepts, not one
method per frontend operation:

- Stronger effects from `lookup_child`, `list_children`, and `read_file`
  where providers return adjacent attrs, adjacent content, or directory facts
  they already learned (via `effects.fs` and `effects.canonical`).
- Directory freshness metadata, separate from file version tokens.
- Batched projection results for domains like DNS where one upstream query
  naturally produces many adjacent files.
- Better negative-result shape, especially to distinguish "valid file with no
  answers" from "invalid file name" and "query failed".

## DNS prefetch shape

DNS is the motivating example for a protocol-neutral prefetch model.

The desired behavior is:

1. Validate the resolver and domain synchronously during lookup or directory
   open.
2. Project static record files immediately so `cd` and `ls` stay fast and
   predictable.
3. Schedule asynchronous prefetch for all record files after a valid domain
   directory is observed.
4. Store prefetched record content through the normal host cache path.
5. If `read(A)` happens before prefetch completes, the read path should use
   prefetched content if available or block on the direct query or in-flight
   prefetch.
6. When async prefetch learns content or sizes, invalidate affected inode attrs
   and content so kernel caches and the view cache do not retain stale `st_size`
   or empty content.
7. Preserve the provider contract: DNS declares facts or hints; the host
   derives scheduling, cache policy, FUSE flags, NFS attributes, and
   invalidation.

The host should never treat blank file content as success unless the provider
has explicitly confirmed an empty answer for that record type. A blank read
caused by "not fetched yet" is a correctness bug.

## Live acceptance testing

The current container demo is useful as a smoke path, but print-only output is
not enough. Provider correctness needs assertion-based live tests.

Recommended structure:

1. Keep smoke and acceptance separate. Smoke proves the mount starts.
   Acceptance proves each provider's visible contract.
2. Use `set -euo pipefail`, `timeout`, `test`, `grep`, `stat`, and `wc` so
   blank output fails hard.
3. Add a mounted test-provider profile in Docker so synthetic `Exact`,
   `NonZero`, `Unknown`, `Dynamic`, `Live`, inline cap, version token, and
   ranged EOF cases run through real FUSE or NFS, not only Rust tests.
4. Run cold and warm cache passes. Clear `/tmp/omnifs-cache`, assert first
   read behavior, assert second read behavior, and inspect cache/log evidence
   where relevant.
5. Capture diagnostics automatically on failure: command transcript,
   `omnifs status`, `/tmp/omnifs.log`, and relevant mount info.
6. Build a bash-tool matrix around the file-attributes design: `stat`, `ls -l`,
   `cat`, `head`, `tail -c`, `wc -c`, `grep`, `find`, `cp`, `tar`, and `diff`
   against known-good outputs.
7. Use stable live fixtures and explicit assertions. DNS should assert record
   output shape, GitHub should assert known paths and fields, and arXiv should
   assert known metadata fields.

Example DNS acceptance checks:

```bash
cd /dns/google.com
ls A AAAA MX NS TXT SOA all raw
test -s A
cat A | grep -E '^A[[:space:]]+'
test -s all
cat all | grep -E '^(A|AAAA|MX|NS|TXT|SOA)[[:space:]]+'
```
