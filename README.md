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

- Node.js and npm for the packaged CLI path
- Docker on Linux or Docker Desktop on macOS for the default packaged runtime
- Native mode uses NFSv4 loopback on macOS and FUSE on Linux when provider WASM components are available on disk
- SSH agent running with a GitHub key loaded if you want repo clone paths under `/github`

### CLI-managed runtime

The normal user flow is owned by the `omnifs` CLI. The CLI stores system settings and `[[mounts]]` in `~/.omnifs/config.toml`, keeps credential data on the host, starts the selected runtime, and opens a shell at the mount root.

```bash
npm install -g @0xff-ai/omnifs
omnifs setup --mode native   # or: omnifs setup --mode docker
omnifs init github
omnifs init linear
omnifs status
omnifs up
omnifs shell
```

GitHub uses device-code OAuth with the bundled public client id and no default write scopes. Linear uses browser PKCE OAuth with the bundled public client id and `read` scope. You do not need to edit configs, copy provider wasm into `~/.omnifs/data/providers`, or set OAuth client id environment variables for the bundled providers. `omnifs up` uses the version-matched runtime image unless you override it with `--image` or `OMNIFS_IMAGE`.

The npm package installs only the native host CLI. It does not pull the Docker image during install; Docker mode pulls and starts the matching runtime image on `omnifs up`. `omnifs setup --mode native` or `omnifs setup --mode docker` persists the selected runtime in `~/.omnifs/config/config.toml`; later runtime commands load that default automatically.

```bash
omnifs setup --mode native
omnifs up
omnifs shell
```

You can inspect or change the same default directly:

```bash
omnifs config runtime --mode native
omnifs config runtime --mode docker
```

Per-command `--mode native` or `--mode docker` remains available as an override. Native mode currently requires provider `.wasm` components in the configured providers directory. Docker mode remains the default packaged runtime until provider sidecars ship through the npm packages.

Use `omnifs logs`, `omnifs logs -f`, and `omnifs down` to inspect and stop the selected runtime.

### Contributor sandbox

`omnifs dev` is the development workflow for this repo. It captures your `gh` token, fetches the Chinook SQLite fixture under `.omnifs-dev/fixtures`, synthesizes dev mount configs from the built-in provider manifests, and launches the selected runtime. If no runtime has been persisted, Docker mode is the default for CI parity; native mode builds provider WASM artifacts and mounts the runtime on the host:

```bash
# Clone the repo
git clone https://github.com/0xff-ai/omnifs
cd omnifs

# Build, materialize, and start Docker mode
omnifs dev
omnifs shell

# Build providers and start native mode
omnifs dev --mode native
omnifs shell --mode native
```

`-y` skips the session confirmation prompt.

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
actions  issues  pulls  repo

# clone the repo just by listing it
> cd /github/ollama/ollama/repo
> ls
CMakeLists.txt     Makefile.sync  cmd        go.mod       llm         openai    server     x
CMakePresets.json  README.md      convert    go.sum       logutil     parser    template
CONTRIBUTING.md    SECURITY.md    discover   harmony      main.go     progress  thinking
[...]

# list open issues
> cd /github/ollama/ollama/issues/open
> ls
10333  10928  11381  11743  12138  12539  12959  13399  13879  14239  14621  15087  15398
10337  10929  11384  11746  12148  12541  12963  13401  13883  14243  14628  15091  15400

## ... and a lot more! play the video above for a walkthrough

###################
## DNS as files
###################

> cd /dns/cloudflare.com
> ls
A  AAAA  CAA  CNAME  MX  NS  SOA  SRV  TXT  all  raw
> cat A
104.16.133.229
> cat /dns/@8.8.8.8/google.com/AAAA
2a00:1450:4003:804::200e
> cat /dns/reverse/1.1.1.1
one.one.one.one.

## poke around!
```

Use `omnifs logs` (`-f` to follow) and `omnifs down` to inspect and stop the dev runtime.

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

omnifs runs as a projected filesystem with a native FUSE frontend on Linux, a native NFSv4 loopback frontend on macOS, and an optional Docker runtime that keeps the Linux FUSE mount inside the container. The architecture has three layers:

```
                                                                      ┌────────────────┐
┌──────────────┐            ┌────────────────────────────┐            │ github.wasm    ├──▶ GitHub
│  your shell  │    FUSE    │         omnifs host        │  callouts  │ linear.wasm    ├──▶ Linear
│  or agent    │ ◀──────▶   │  /github  /linear  /arxiv  │ ◀-──────▶  │ arxiv.wasm     ├──▶ arXiv
│              │   files    │             ...            │            │ ...            ├──▶ ...
└──────────────┘            └────────────────────────────┘            │                │
                                                                      └────────────────┘
```

**Wasm providers** are WebAssembly components. Each provider projects a domain (GitHub, Linear, S3, whatever) into the filesystem namespace. Drop a `.wasm` into `~/.omnifs/data/providers/` and it mounts.

**Callout runtime** means providers never touch the network or Git directly. They describe what they need ("fetch this API endpoint", "clone this repo"), and the host executes. This keeps providers sandboxed and lets the host manage caching, rate limits, and concurrency.

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
| `/github/{owner}/{repo}/repo/`                            | Browse the repo tree (cloned on demand via SSH) |
| `/github/{owner}/{repo}/issues/open/`                    | List open issues                                |
| `/github/{owner}/{repo}/issues/all/`                     | List all issues                                 |
| `/github/{owner}/{repo}/issues/{filter}/{n}/title`        | Issue title                                     |
| `/github/{owner}/{repo}/issues/{filter}/{n}/body`         | Issue body (markdown)                           |
| `/github/{owner}/{repo}/issues/{filter}/{n}/state`        | Issue state                                     |
| `/github/{owner}/{repo}/issues/{filter}/{n}/comments/{i}` | Individual comment                              |
| `/github/{owner}/{repo}/pulls/{filter}/{n}/diff`            | PR diff                                         |
| `/github/{owner}/{repo}/actions/runs/{id}/status`         | CI run status                                   |
| `/github/{owner}/{repo}/actions/runs/{id}/log`            | CI run log                                      |

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
| `/dns/{domain}/all`                 | All common record types                                     |
| `/dns/{domain}/raw`                 | dig-style output                                            |
| `/dns/@{resolver}/{domain}/{record}` | Query via a specific resolver (e.g., `@google`, `@1.1.1.1`) |
| `/dns/{ip}`                          | Reverse DNS lookup (PTR)                                    |
| `/dns/reverse/{ip}`                 | Reverse DNS lookup (alternate path)                         |
| `/dns/resolvers`                    | List configured resolvers                                   |

### arXiv (`/arxiv`)

Per-paper subtrees under `/arxiv/papers/{id}/` are version-first. The paper directory lists `@latest` plus numbered `vN` directories, and the same shape is mirrored under category membership paths.

| Path                                                  | Content                                                       |
| ----------------------------------------------------- | ------------------------------------------------------------- |
| `/arxiv/papers/{id}/`                                 | Per-paper subtree (any arXiv id, e.g. `1706.03762`)           |
| `/arxiv/papers/{id}/@latest/paper.pdf`                | Latest version PDF                                            |
| `/arxiv/papers/{id}/@latest/source.tar.gz`            | Latest version source bundle                                  |
| `/arxiv/papers/{id}/@latest/paper.atom`               | Raw upstream Atom feed                                        |
| `/arxiv/papers/{id}/@latest/paper.json`               | Rendered metadata for the latest version                      |
| `/arxiv/papers/{id}/v{n}/paper.pdf`                   | Version-pinned PDF                                            |
| `/arxiv/papers/{id}/v{n}/source.tar.gz`               | Version-pinned source bundle                                  |
| `/arxiv/papers/{id}/v{n}/paper.atom`                  | Raw Atom backing the paper object                             |
| `/arxiv/papers/{id}/v{n}/paper.json`                  | Rendered metadata with version-specific resource URLs         |
| `/arxiv/categories/{cat}/papers/`                     | Most-recent papers in `cat` by submitted date                 |
| `/arxiv/categories/{cat}/papers/{id}/@latest/...`     | Category alias for the same paper version family              |

## What's coming

### Core omnifs

- Write-back via git push (mutations through staging transactions)
- Better caching (hot-path memoization, negative caching, smarter invalidation)
- Background indexing for large trees and expensive projections
- Search across projected content, metadata, and repo history
- Tracing and observability for provider calls, cache behavior, and frontend latency
- Better prefetching and pagination strategies for large orgs and repos
- Persistent inode stability across remounts
- Offline-friendly local snapshots and replayable sync
- Mutation workflows beyond read-only browsing
- Windows support

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
