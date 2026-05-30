---
title: Using omnifs with AI agents
description: Paths are the universal API for agents — no SDK, no auth flow, no pagination. Open a path and read.
---

omnifs is built for AI agents. Instead of integrating a dozen SDKs and managing
a dozen auth flows, an agent reads files. Any agent that can read a file can use
any service omnifs projects.

```bash
cat /github/myorg/myrepo/issues/4242/body
cat /dns/mycompany.com/MX
cat /arxiv/papers/1706.03762/metadata.json
```

## Paths as the universal API

Every service speaks the same language: paths and files. This removes the parts
of integration that usually break agents.

- **No SDK per service.** One interface for everything — the filesystem.
- **No auth flow in the agent.** Credentials are managed by omnifs, out of band.
  The agent never sees a token or runs an OAuth dance.
- **No pagination, no rate-limit handling.** The filesystem layer abstracts it
  away; the agent just reads the next path.
- **Stable contracts.** Paths do not change between API versions.

## Why stable path contracts matter

An agent's behavior is only as reliable as the interface it targets. SDK
signatures, JSON shapes, and endpoint versions drift; omnifs paths do not. A
prompt or tool that reads `/github/<owner>/<repo>/issues/<n>/body` keeps working
as the underlying API evolves, because the path is the contract. That stability
is what lets you bake omnifs paths into agent instructions and trust them across
runs.

## Practical examples

An agent investigating a bug, cross-referencing a paper, or checking a
misconfiguration runs the same kind of plain reads a human would.

```bash
# Triage an issue and the related PRs
cat /github/myorg/myrepo/issues/4242/body
cat /github/myorg/myrepo/issues/4242/comments
ls  /github/myorg/myrepo/pulls

# Pull a referenced paper's metadata
cat /arxiv/papers/2017.06464/metadata.json

# Diagnose a DNS / email misconfiguration
cat /dns/mycompany.com/MX
cat /dns/mycompany.com/TXT
```

Because the paths are real files, the agent can compose them with the rest of
the toolbox — `grep -r` across a cloned repo, `jq` over a metadata file, `diff`
between two DNS answers — without any service-specific code.

## Writing back (work in progress)

Write access — creating issues, committing code — is in progress and follows the
same model: write to a file, and the change propagates. The read model stays
read-only; mutations are explicit and opt-in rather than implicit edits to
projected files.

:::caution
Write support is not generally available yet. Today, treat omnifs paths as a
read interface for agents. See [Authentication](/guides/authentication/) for how
read-only scopes keep the blast radius small.
:::
