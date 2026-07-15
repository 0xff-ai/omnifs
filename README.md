<div align="center">
<p align="center">
  <img src="docs/assets/omnifs-hero.png" width="960" alt="omnifs">
</p>

<h1 align="center"><b>omnifs</b></h1>
<h4 align="center">open a path, read the world.</h4>
<p align="center"><a href="#quickstart">quickstart</a> | <a href="https://omnifs.dev">homepage</a> | <a href="https://omnifs.dev/start">docs</a> | <a href="#things-to-try">things to try</a> | <a href="#providers">providers</a> | <a href="#how-it-works">how it works</a></p>
</div>

omnifs projects external systems into local filesystem paths. GitHub, DNS, arXiv, Docker, Linear, SQLite, Kubernetes, web pages, and Oura become directories and files you can `cd`, `ls`, `cat`, `grep`, `find`, `jq`, and script against.

The goal is simple: if a tool can read files, it can read the outside world without learning another SDK, auth flow, pagination model, or response schema.

> Alpha status: omnifs is real and usable, but the read surface is still early. The daemon runs natively on Linux and macOS. Frontends are independent whole-namespace access surfaces managed with imperative `frontend enable`, `disable`, and `restart` commands. Docker and libkrun deliver FUSE only. Frontends attach over the wire protocol and carry no providers or credentials.

<p align="center">
  <img src="https://github.com/user-attachments/assets/b9598ece-e772-4fdc-b5c7-8ad5ba26d39d" alt="omnifs demo" width="960">
</p>

## Quickstart

omnifs is written in Rust. We ship prebuilt Linux and macOS binaries through the npm registry, so the npm installation path needs Node.js and npm. Docker or OrbStack is needed only for the optional Docker-hosted FUSE frontend; macOS can use libkrun instead.

```bash
npm install -g @0xff-ai/omnifs
omnifs setup --providers github
omnifs status
```

`omnifs setup` configures exact embedded providers, starts the host-native daemon, and enables the accepted platform defaults as one imperative first-run flow. Use `omnifs setup --no-up` to configure mounts only, or use `omnifs mount add <provider>`, `omnifs up`, and `omnifs frontend enable` for separate operations. Host frontends are ordinary mounted paths; use `omnifs frontend shell` only for a Docker or libkrun frontend.

---

For a direct, scriptable path, create mounts one at a time with `omnifs mount add <provider>`. Each command records a mount-spec commit in the Git repository at `~/.omnifs/mounts/`; `omnifs up` starts or restarts the daemon against that committed revision. `omnifs apply` is an exact alias of `omnifs up`.

```bash
omnifs mount add github
omnifs mount add dns
omnifs up
omnifs frontend enable fuse --runtime host --location "$HOME/omnifs"
omnifs status
cd "$HOME/omnifs"
```

---

Useful commands:

```bash
omnifs status      # runtime, mount, and auth state
omnifs frontend ls # observed whole-namespace frontend runners and attachments
omnifs logs -f     # follow daemon logs
omnifs inspect     # live TUI for namespace, provider, cache, and callout activity
omnifs frontend disable fuse --runtime host # stop a frontend runner
omnifs down        # stop the daemon
```

Frontend lifecycle is imperative. Host locations must be absolute; Docker and libkrun own their guest location.

```bash
omnifs frontend enable nfs --runtime host --location "/Users/me/omnifs"
omnifs frontend enable fuse --runtime docker
omnifs frontend restart fuse --runtime docker
omnifs frontend disable fuse --runtime docker
omnifs frontend shell fuse --runtime docker
```

Every attached frontend exposes every configured mount. Provider installation and mount creation are separate, and `omnifs up` applies each mount spec's exact provider pin without selecting upgrades.

For automation, select one invocation-owned output contract. JSON prints one envelope and keeps resource collections plural; JSONL uses the same terminal result or error envelope with its stream-record discriminator. Live logs and Inspector records remain line streams.

```bash
omnifs --output json status | jq '.result.frontends[] | {filesystem, runtime, location, scope}'
omnifs --output json mount ls | jq '.result.mounts[]'
```

## Things to try

Once you are inside any frontend mount, use normal shell tools.

```bash
# GitHub
cd /github/ollama/ollama
ls
cat repo.json
cat issues/open/12959/title
cat pulls/all/9585/diff.patch
# repository trees are cloned on demand
cd repo && ls

# DNS
cat /dns/cloudflare.com/A
cat /dns/@google/google.com/AAAA
cat /dns/openai.com/TXT
cat /dns/reverse/1.1.1.1

# arXiv -- "Attention is all you need"
ls /arxiv/papers/1706.03762
cat /arxiv/papers/1706.03762/@latest/paper.json | jq .title

# Docker
cat /docker/system/version.json | jq .
cat /docker/containers.json | jq .
ls /docker/containers/running

# Linear -- requires a Linear API key
ls /linear/teams
cat /linear/teams/ENG/issues/open/ENG-123/title

# SQLite -- download an example db and explore the data
wget -O /tmp/chinook.sqlite https://github.com/lerocha/chinook-database/raw/refs/heads/master/ChinookDatabase/DataSources/Chinook_Sqlite.sqlite
omnifs mount add db # provide path: /tmp/chinook.sqlite
ls /db/tables
cat /db/tables/Album/schema.sql
cat /db/tables/Album/sample.json | jq .
```

<details>
<summary>SSH agent troubleshooting</summary>

Check the host before opening repo tree paths:

```bash
echo "$SSH_AUTH_SOCK"
ssh-add -L
ssh -T git@github.com
```

</details>

## Why paths

APIs are good boundaries for applications. They are a bad default interface for every script, terminal session, CI job, editor, and agent that only needs to read state.

omnifs makes the path the interface:

```text
/github/ollama/ollama/issues/open/12959/title
/docker/containers/running/{name}/state
/arxiv/papers/1706.03762/@latest/paper.json
/dns/cloudflare.com/TXT
```

That gives existing tools a common substrate. `grep -r`, `find`, `jq`, `tar`, `diff`, `head`, `tail`, and editors can all operate without provider-specific clients. Agents get the same benefit: open a path and read bytes.

The current surface is read-only. Write-back is designed around explicit staged transactions, but projected issue, PR, container, and DNS files are not directly writable today.

## Providers

| Provider | Mount | What it projects |
| --- | --- | --- |
| GitHub | `/github` | Users, orgs, repos, issues, pull requests, Actions runs, diffs, and repo trees cloned on demand |
| DNS | `/dns` | DNS-over-HTTPS records, resolver-scoped queries, raw answers, and reverse lookups |
| arXiv | `/arxiv` | Paper version families, PDFs, source archives, metadata, and category paper listings |
| Docker | `/docker` | Docker daemon system state, container listings, per-container inspect output, state, and summaries |
| Linear | `/linear` | Teams and issues, with title, state, priority, assignee, and description files |
| SQLite | `/db` | Read-only SQLite metadata, table schemas, indexes, row counts, and samples |
| Kubernetes | `/k8s` | Live namespaces, cluster resources, manifests, status, events, and pod logs |
| Web | `/web` | Allowed HTTPS pages as readable Markdown and raw response bytes |
| Oura | `/oura` | Daily health, sleep, readiness, workout, heart-rate, and ring-battery data |

### GitHub

| Path | Content |
| --- | --- |
| `/github/{owner}` | Repositories for a user or organization |
| `/github/{owner}/{repo}` | Repository surface |
| `/github/{owner}/{repo}/repo/` | Source tree, cloned on demand via SSH |
| `/github/{owner}/{repo}/issues/{open,all}/` | Issue listings |
| `/github/{owner}/{repo}/issues/{filter}/{n}/title` | Issue title |
| `/github/{owner}/{repo}/issues/{filter}/{n}/body` | Issue body |
| `/github/{owner}/{repo}/pulls/{filter}/{n}/diff.patch` | Pull request diff |
| `/github/{owner}/{repo}/actions/runs/{id}/status` | Actions run status |
| `/github/{owner}/{repo}/actions/runs/{id}/log` | Actions run log |

### DNS

| Path | Content |
| --- | --- |
| `/dns/{domain}/A` | A records |
| `/dns/{domain}/AAAA` | AAAA records |
| `/dns/{domain}/MX` | MX records |
| `/dns/{domain}/TXT` | TXT records |
| `/dns/{domain}/all` | Common record types |
| `/dns/{domain}/raw` | Dig-style output |
| `/dns/@{resolver}/{domain}/{record}` | Query through a named or IP resolver |
| `/dns/reverse/{ip}` | Reverse lookup |
| `/dns/resolvers` | Configured resolvers |

### arXiv

| Path | Content |
| --- | --- |
| `/arxiv/papers/{id}/` | Paper version family |
| `/arxiv/papers/{id}/@latest/paper.pdf` | Latest version PDF |
| `/arxiv/papers/{id}/@latest/source.tar.gz` | Latest version source bundle |
| `/arxiv/papers/{id}/@latest/paper.atom` | Raw upstream Atom feed |
| `/arxiv/papers/{id}/@latest/paper.json` | Rendered metadata |
| `/arxiv/papers/{id}/v{n}/paper.pdf` | Version-pinned PDF |
| `/arxiv/categories/{cat}/papers/` | Recent papers in a category |
| `/arxiv/categories/{cat}/papers/{id}/@latest/...` | Category alias for the same paper version family |

### Docker, Linear, and SQLite

| Path | Content |
| --- | --- |
| `/docker/system/version.json` | Docker daemon version |
| `/docker/containers.json` | Container listing |
| `/docker/containers/{by-name,by-id,running,stopped}/` | Container indexes |
| `/docker/containers/running/{name}/state` | Live container state |
| `/docker/containers/running/{name}/inspect.json` | Docker inspect JSON |
| `/linear/teams/` | Linear teams by key |
| `/linear/teams/{KEY}/issues/{open,all}/` | Team issue listings |
| `/linear/teams/{KEY}/issues/{filter}/{KEY-N}/description.md` | Issue description |
| `/db/meta/info.json` | SQLite database metadata |
| `/db/tables/{table}/schema.sql` | Table schema |
| `/db/tables/{table}/sample.json` | Sample rows |

## How it works

The host-native daemon owns providers, credentials, caching, callouts, and one shared namespace. FUSE and NFS frontends in host, Docker, or libkrun runtimes all expose that same tree via the wire protocol.

```text
                                                                  +----------------+
+-------------+          +-----------------------------+          | github.wasm    | -> GitHub
| shell, app, | FUSE/NFS |        omnifs daemon        | callouts | dns.wasm       | -> DoH
| CI, agent   | <------> | /github /dns /arxiv ...     | <------> | docker.wasm    | -> Docker socket
|             |  files   | cache, auth, git, network   |          | linear.wasm    | -> Linear
+-------------+          +-----------------------------+          +----------------+
```

Providers are WebAssembly components implementing the [`omnifs:provider` WIT interface](crates/omnifs-wit/wit/provider.wit). Providers are self-contained: they declare identity, capabilities, config metadata, and auth via `#[omnifs_sdk::provider]` annotations, which the build assembles into the `omnifs.provider-metadata.v1` Wasm custom section. A provider's main job is to answer filesystem operations via entrypoint methods `lookup_child`, `list_children`, and `read_file`.

Providers do not hold tokens, open sockets, or run Git themselves. They await typed host callouts such as HTTP fetches, blob downloads, archive opens, or repo tree handoffs. The host executes those imports, attaches credentials at the boundary, enforces declared capabilities, and owns caching.

The cache is host-owned plain byte storage. Providers can return canonical upstream bytes and derived filesystem entries together, so one upstream payload can populate multiple files and child entries. Invalidations come from explicit provider effects and runtime events.

## Development workflows

Use `just dev` when working from this repository. It builds provider WASM and the native CLI, renders the built-in dev mounts and credentials under `~/.omnifs-dev`, starts the host-native daemon and fixtures, attaches the slim Docker-hosted FUSE frontend, and opens a shell at `/omnifs`.

```bash
git clone https://github.com/0xff-ai/omnifs
cd omnifs
just dev -y
# opens the attached frontend shell at /omnifs
```

Refresh generated and formatted artifacts:

```bash
just refresh
```

For runtime behavior, validate through the host daemon and attached frontend:

```bash
just dev -y
omnifs status
tail -n 80 ~/.omnifs-dev/cache/daemon.log
```

## Roadmap

### ✅ Working today

- FUSE (Linux) and read-only NFSv4 loopback (macOS) frontends in host, Docker, or libkrun runtimes; enabled explicitly with the frontend lifecycle commands.
- A host CLI on npm that handles mounts, auth, lifecycle, logs, status, and inspection.
- Sandboxed Wasm providers that can only reach the network, Git, sockets, and files the host hands them.
- Host-held credentials, layered caching, and `omnifs inspect` for a live view of what the runtime is doing.
- Nine live providers: GitHub, DNS, arXiv, Docker, Linear, SQLite, Kubernetes, Web, and Oura.

### 🚧 In progress

- Making the provider SDK nicer to write against, especially for object-shaped providers.
- Letting providers build paths from their registered routes instead of hand-formatting strings.
- Caching polish: clearer traces, bounded disk usage, and identities that survive a remount.
- Better behavior under stuck reads and aggressive directory walkers (shells, prompt tools, crawlers).
- Smoother mount, auth, status, and `doctor` output, plus stronger frontend acceptance tests.
- Provider reference docs generated straight from each provider's manifest and routes.

### 🔭 Planned

- Write support: stage your intent first, then apply it upstream.
- Many more providers, including object stores, Postgres, Redis, Slack, Discord, Google Drive, Gmail, Notion, Stripe, Cloudflare, Vercel, and Telegram.
- A real provider ecosystem: standalone packaging, a community catalog, authoring docs, and sidecars for providers that need native dependencies.
- Additional mount surfaces beyond Linux FUSE and macOS NFSv4, plus passthrough for host-backed subtrees.
- Easier install and slimmer packaging: Homebrew or shell installers and smaller frontend images for docker and libkrun delivery.
- Offline cache-backed startup and browsing with `omnifs up --offline`, plus background indexing, semantic search, and DNS prefetch.
- Trust and safety: signed provider manifests, tighter sandboxing for host-run tools, and metered filesystem access.

## License

MIT OR Apache-2.0
