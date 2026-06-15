---
title: The trust model
description: The one trusted control plane and one untrusted boundary that structure omnifs security.
---

# The trust model

omnifs has one trusted control plane and one untrusted boundary, and they are not the same line.

The trusted control plane is the host: the CLI, the daemon, and the container they run. These run code you installed and are trusted with host authority, including the SSH agent, selected secrets, declared preopens, and sometimes Docker access. Sharing runtime state among them is expected. The daemon and CLI read and write the same credential store and the same runtime home by design.

The untrusted boundary is provider code. Providers are `wasm32-wasip2` components and stay constrained by the sandbox: no ambient network, no ambient filesystem, no credentials, and only the host-mediated callouts and host-enforced capabilities they were granted. This boundary is the one that matters. A provider is the one part of the system assumed to be hostile, and every other guarantee is built to hold even if it is.

The practical consequence: do not design boundaries around hiding host state from the container, because the container already runs trusted host code. Design them around what a provider can reach. Granting more filesystem authority to the host runtime is fine. Granting more authority to provider WASM is a change to the security model.
