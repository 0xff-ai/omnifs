---
title: "Runtime grants"
description: "The grants the host uses after mount resolution: domains, Git repo patterns, Unix sockets, WASI preopens, auth injection, and memory declarations."
---

Runtime grants are the host's resolved view of what a provider instance may ask it to do.

| Grant | Meaning |
|---|---|
| Domain | HTTPS host allowed for callouts. Built-in manifests should use exact hosts; the runtime checker also accepts an explicit `*` wildcard grant. |
| Git repo | Pattern for Git handoff. |
| Unix socket | Exact Unix socket path allowed for socket-backed callouts. |
| Preopened path | Host path exposed into the provider sandbox through WASI. |
| Auth injection | Domains and header shape for host-owned credentials. |
| Memory declaration | Provider's requested memory budget metadata. |
| Blob limits | Maximum fetch/read blob byte declarations used by blob handling. |

Runtime grants are checked by the host. They are not a promise that a provider is harmless. Docker socket access and local preopens remain high-value review surfaces.
