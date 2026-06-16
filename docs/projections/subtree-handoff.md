---
title: Subtree handoff
description: How a provider can hand an entire path subtree to a host-managed backing tree, such as a cloned repository.
---

Most provider paths are answered by provider dispatch: the host calls `lookup-child`, `list-children`, or `read-file`, and the provider returns directory entries, file bytes, or effects. Some paths are better served by a real backing tree.

Git repository contents are the clearest case. Once a provider resolves a repository, the host can clone or open it, bind the resulting directory into the projection, and let normal filesystem traversal continue from that backing path. The provider does not mount host directories itself. It names the tree; the host owns the backing path.

## The three-piece model

A subtree handoff has three pieces:

1. The provider recognizes a path that should become a real tree.
2. The provider returns a `tree-ref` through the WIT result instead of file bytes or directory entries.
3. The host maps that `tree-ref` to a backing path and routes later filesystem operations there.

## Why it exists

Some resources are already filesystem-shaped. A Git repository has nested paths, files, executable bits, symlinks, and tool expectations that would be awkward to re-project file by file. Subtree handoff lets omnifs preserve the path interface without forcing every byte through provider WIT calls.

## The GitHub split

For GitHub, issue and pull-request metadata are projected as provider paths. Repository contents use subtree handoff. That split matters:

- Issue fields are cached as object and view data.
- Repository files behave like real checked-out files.
- `grep`, `find`, `tar`, and editors operate on the backing tree without any special handling.
- Clone transport and SSH agent forwarding remain host and runtime responsibilities.

## Authority boundary

The host owns clone management, backing paths, bind mounts, and later traversal. The provider owns the path decision and the `tree-ref` it returns. Providers do not receive ambient disk access and do not read host private key material. Git clone currently uses SSH with a forwarded agent socket.

## Relationship to the object cache

Subtree handoff is not the object and view cache path. Object-shaped results can store canonical bytes and rendered view leaves in the cache. A backing tree is different: it is a host-managed subtree the filesystem traverses directly. If a resource should be rendered into small field files, use the object path. If a resource already is a tree of files, subtree handoff is usually the better fit.
