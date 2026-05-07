<div align="center">
<p align="center">
  <img src="https://github.com/user-attachments/assets/43af533a-4db1-46f5-a7b5-bbcb75be0786" width="960" alt="omnifs">
</p>

<h1 align="center"><b>omnifs</b></h1>
<h4 align="center">the universe, mounted on your filesystem.</h4>
<p align="center"><a href="#quickstart">Quickstart</a> | <a href="#explore">Examples</a> | <a href="#providers">Providers</a></p>
</div>

omnifs mirrors the entire world into your local filesystem. GitHub repos, Hugging Face models, Kubernetes clusters, Slack channels, arXiv papers, and more as paths you can `cd`, `ls`, `cat`, and `grep`.

Plan 9 was right, just 40 years early. Everything is a file. The world moved to APIs; omnifs moves it back to paths, for humans and agents alike.

> _🚧 very alpha!_

<p align="center">
  <img src="https://github.com/user-attachments/assets/b9598ece-e772-4fdc-b5c7-8ad5ba26d39d" alt="omnifs demo" width="960">
</p>

## Quickstart

### Prerequisites

- Docker
- SSH agent running with a GitHub key loaded
- `gh` CLI (for generating a token)

### Published Docker image

Run the published image directly:

```bash
# Run the container
# Automatically picks up your GitHub auth token from the gh cli, and wires the SSH auth sock for git ssh clones
docker run -d \
  --name omnifs \
  --device /dev/fuse \
  --cap-add SYS_ADMIN \
  --security-opt apparmor:unconfined \
  -e GITHUB_TOKEN="$(gh auth token)" \
  -e SSH_AUTH_SOCK=/ssh-agent \
  -e GIT_SSH_COMMAND='ssh -F /dev/null -o StrictHostKeyChecking=accept-new' \
  -v "$SSH_AUTH_SOCK:/ssh-agent" \
  ghcr.io/raulk/omnifs:latest

# Enter the container's shell
docker exec -it omnifs /bin/zsh
```

Use `docker logs omnifs` to inspect logs and `docker rm -f omnifs` to stop the container.

### Local Docker image with Compose

```bash
# Clone the repo
git clone https://github.com/raulk/omnifs
cd omnifs

# Feed secrets
mkdir -p .secrets
gh auth token > .secrets/github_token

# Run docker compose
docker compose up --build -d
docker compose exec omnifs /bin/zsh
```

### Explore

```bash
#####################
## GitHub as files
###################

# List repos in user/org
> cd /github/torvalds
> ls
1590A       GuitarPedal       libdc-for-dirk  linux       subsurface-for-dirk  uemacs
AudioNoise  HunspellColorize  libgit2         pesconvert  test-tlb

# cd into a repo
> cd /github/ollama/ollama
> ls
_actions  _issues  _prs  _repo

# clone the repo just by listing it
> cd /github/ollama/ollama/_repo
> ls
CMakeLists.txt     Makefile.sync  cmd        go.mod       llm         openai    server     x
CMakePresets.json  README.md      convert    go.sum       logutil     parser    template
CONTRIBUTING.md    SECURITY.md    discover   harmony      main.go     progress  thinking
[...]

# list open issues
> cd /github/ollama/ollama/_issues/_open
> ls
10333  10928  11381  11743  12138  12539  12959  13399  13879  14239  14621  15087  15398
10337  10929  11384  11746  12148  12541  12963  13401  13883  14243  14628  15091  15400

## ... and a lot more! play the video above for a walkthrough

###################
## DNS as files
###################

> cd /dns/cloudflare.com
> ls
A  AAAA  CAA  CNAME  MX  NS  SOA  SRV  TXT  _all  _raw
> cat A
104.16.133.229
> cat /dns/@8.8.8.8/google.com/AAAA
2a00:1450:4003:804::200e
> cat /dns/_reverse/1.1.1.1
one.one.one.one.

## poke around!
```

Use `docker compose logs omnifs` to inspect logs and `docker compose down` to stop the source-built container.

<details>
<summary>SSH agent troubleshooting</summary>

omnifs clones repos over SSH inside the container using your forwarded agent socket. This does not copy your private key into the container, but it does let the container ask your agent to sign while the socket is mounted.

Verify your setup:

```bash
echo "$SSH_AUTH_SOCK"
ssh-add -L
ssh -T git@github.com
```

</details>

## For agents

Agents should not have to deal with APIs. If you can read a file, you can read the world. No SDK to install, no authentication flow to implement, no pagination to manage. Just open a path and read. Write files, commit, push to sync back. The filesystem is the universal API.

## How it works

omnifs runs as a FUSE filesystem on Linux (macOS and Windows planned). The architecture has three layers:

```
                                                                      ┌────────────────┐
┌──────────────┐            ┌────────────────────────────┐            │ github.wasm    ├──▶ GitHub
│  your shell  │    FUSE    │         omnifs host        │   effects  │ linear.wasm    ├──▶ Linear
│  or agent    │ ◀──────▶   │  /github  /linear  /arxiv  │ ◀-──────▶  │ arxiv.wasm     ├──▶ arXiv
│              │   files    │             ...            │            │ ...            ├──▶ ...
└──────────────┘            └────────────────────────────┘            │                │
                                                                      └────────────────┘
```

**Wasm providers** are plugins compiled to WebAssembly components. Each provider projects a domain (GitHub, Linear, S3, whatever) into the filesystem namespace. Drop a `.wasm` into `~/.omnifs/plugins/` and it mounts.

**Effect-based runtime** means providers never touch the network or Git directly. They describe what they need ("fetch this API endpoint", "clone this repo"), and the host executes. This keeps providers sandboxed and lets the host manage caching, rate limits, and concurrency.

**Git-backed reconciliation (WIP)** means writes work through Git. Edit files in a transaction directory, then rename it to `commit/` to execute. The provider translates that into API calls. Everything stays auditable, revertible, and familiar.

## Providers

| Provider   | Mount     | Description                                                              |
| ---------- | --------- | ------------------------------------------------------------------------ |
| **GitHub** | `/github` | Browse repos, issues, PRs, CI runs, and diffs as files                   |
| **DNS**    | `/dns`    | Query DNS records via DNS-over-HTTPS                                     |
| **arXiv**  | `/arxiv`  | Browse arXiv papers by id, category, author, or search; PDFs and source |

### GitHub (`/github`)

| Path                                                       | Content                                         |
| ---------------------------------------------------------- | ----------------------------------------------- |
| `/github/{owner}`                                          | List repos for a user or org                    |
| `/github/{owner}/{repo}/_repo/`                            | Browse the repo tree (cloned on demand via SSH) |
| `/github/{owner}/{repo}/_issues/_open/`                    | List open issues                                |
| `/github/{owner}/{repo}/_issues/_all/`                     | List all issues                                 |
| `/github/{owner}/{repo}/_issues/{filter}/{n}/title`        | Issue title                                     |
| `/github/{owner}/{repo}/_issues/{filter}/{n}/body`         | Issue body (markdown)                           |
| `/github/{owner}/{repo}/_issues/{filter}/{n}/state`        | Issue state                                     |
| `/github/{owner}/{repo}/_issues/{filter}/{n}/comments/{i}` | Individual comment                              |
| `/github/{owner}/{repo}/_prs/{filter}/{n}/diff`            | PR diff                                         |
| `/github/{owner}/{repo}/_actions/runs/{id}/status`         | CI run status                                   |
| `/github/{owner}/{repo}/_actions/runs/{id}/log`            | CI run log                                      |

### DNS (`/dns`)

| Path                                 | Content                                                     |
| ------------------------------------ | ----------------------------------------------------------- |
| `/dns/{domain}/A`                    | A records                                                   |
| `/dns/{domain}/AAAA`                 | AAAA records                                                |
| `/dns/{domain}/MX`                   | MX records                                                  |
| `/dns/{domain}/NS`                   | NS records                                                  |
| `/dns/{domain}/TXT`                  | TXT records                                                 |
| `/dns/{domain}/CNAME`                | CNAME records                                               |
| `/dns/{domain}/SOA`                  | SOA record                                                  |
| `/dns/{domain}/_all`                 | All common record types                                     |
| `/dns/{domain}/_raw`                 | dig-style output                                            |
| `/dns/@{resolver}/{domain}/{record}` | Query via a specific resolver (e.g., `@google`, `@1.1.1.1`) |
| `/dns/{ip}`                          | Reverse DNS lookup (PTR)                                    |
| `/dns/_reverse/{ip}`                 | Reverse DNS lookup (alternate path)                         |
| `/dns/_resolvers`                    | List configured resolvers                                   |

### arXiv (`/arxiv`)

Per-paper subtrees under `/arxiv/papers/{id}/` are mirrored under every scope (categories, authors, search), so the same `paper.pdf`, `metadata.json`, `versions/v{n}/...` shape works under `/arxiv/categories/{cat}/{ym}/{id}/...` and friends.

| Path                                                  | Content                                                       |
| ----------------------------------------------------- | ------------------------------------------------------------- |
| `/arxiv/papers/{id}/`                                 | Per-paper subtree (any arXiv id, e.g. `1706.03762`)           |
| `/arxiv/papers/{id}/paper.pdf`                        | Latest version PDF                                            |
| `/arxiv/papers/{id}/source.tar.gz`                    | Latest version source bundle                                  |
| `/arxiv/papers/{id}/metadata.json`                    | Title, authors, abstract, categories, comment, links          |
| `/arxiv/papers/{id}/links.json`                       | Resolved arXiv URLs for this paper / version                  |
| `/arxiv/papers/{id}/versions/v{n}/{paper.pdf,…}`      | Same files for a specific version                             |
| `/arxiv/categories/{cat}/{YYYY-MM}/`                  | Papers in `cat` posted in that month (e.g. `cs.AI/2024-01`)   |
| `/arxiv/categories/{cat}/new/{n}/`                    | Most-recent window `n ∈ 0..=14` (newest first by submitted date) |
| `/arxiv/categories/{cat}/updated/{n}/`                | Most-recent window by last-updated date                       |
| `/arxiv/categories/{cat}/by-author/{author}/`         | Papers in `cat` by `author`                                   |
| `/arxiv/authors/{author}/`                            | Papers by author with the same `new`/`updated`/`by-category` axes |
| `/arxiv/search/{query}/`                              | arXiv search results (URL-encoded query)                      |

## What's coming

### Core omnifs

- Write-back via git push (mutations through staging transactions)
- Better caching (hot-path memoization, negative caching, smarter invalidation)
- Background indexing for large trees and expensive projections
- Search across projected content, metadata, and repo history
- Tracing and observability for provider calls, cache behavior, and FUSE latency
- Better prefetching and pagination strategies for large orgs and repos
- Persistent inode stability across remounts
- Offline-friendly local snapshots and replayable sync
- Mutation workflows beyond read-only browsing
- macOS and Windows support

### Provider roadmap

| Provider             | What it could project                                                                             |
| -------------------- | ------------------------------------------------------------------------------------------------- |
| GitHub               | Commits, branches, reviews, checks, releases, and discussion state                                |
| Hugging Face         | Models, datasets, spaces, cards, files, versions, and download metadata as browsable trees        |
| Linear               | Teams, projects, issues, cycles, comments, labels, and workflow state with draftable mutations    |
| DNS                  | Zones, records, history, propagation state, and provider-backed change transactions               |
| S3 and object stores | Buckets, prefixes, object metadata, versions, lifecycle rules, and event streams                  |
| OCI registries       | Images, tags, manifests, layers, SBOMs, and signature material as mountable content               |
| Kubernetes           | Clusters, namespaces, workloads, logs, events, and live resource views                            |
| Postgres and SQLite  | Schemas, tables, rows, views, and queryable virtual files for inspection and export               |
| Slack and Discord    | Channels, threads, message history, attachments, and searchable conversation snapshots            |

## License

MIT OR Apache-2.0
