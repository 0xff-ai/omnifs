# AGENTS.md

Repository-local guidance for working in `omnifs`.

## Scope

- This repo is currently **Linux-only**.
- Do not reintroduce macOS-specific mount behavior, `diskutil`, or macFUSE assumptions unless explicitly requested.
- The primary supported workflow is the **container workflow** in `compose.yaml`.

## Current workflow

Use these commands:

```bash
mkdir -p .secrets
gh auth token > .secrets/github_token
docker compose up --build -d
docker compose exec omnifs /bin/zsh
docker compose logs omnifs
docker compose down
```

Behavior:

- `docker compose up --build -d` builds the image, starts the named container, mounts omnifs at `/omnifs`, and exposes a top-level symlink (`/<mount>`) inside the container for every configured provider in `~/.omnifs/providers/*.json` (currently `/arxiv`, `/dns`, `/github`).
- `docker compose exec omnifs /bin/zsh` opens an interactive `zsh` shell in the running container.
- `docker compose logs omnifs` prints the container output. Runtime logs also remain available at `/tmp/omnifs.log` inside the container.
- `docker compose down` removes the container.

`just` remains available as a wrapper for the same container flow and as the task runner for build and test recipes.

Do not add alternate local mount recipes unless explicitly requested.

## Auth and cloning

Current auth model:

- GitHub API auth for the Docker Compose path comes from a mounted secret file at `.secrets/github_token`, exposed in the container as `/run/secrets/github_token`.
- The baked provider config supports both `token_file` and `token_env`; Compose uses the secret file, while `just start` can still pass `GITHUB_TOKEN`.
- Git clone currently uses SSH:
  - remote format: `git@github.com:<owner>/<repo>.git`
  - auth comes from forwarded `SSH_AUTH_SOCK`
  - do not mount host private keys into the container

Container startup requires:

- host `gh auth token` works so `.secrets/github_token` can be created, or `GITHUB_TOKEN` is already set if using `just start`
- host `SSH_AUTH_SOCK` is set
- host SSH agent has a usable GitHub key loaded

Useful checks on the host:

```bash
test -s .secrets/github_token || gh auth token > .secrets/github_token
ssh-add -L
ssh -T git@github.com
```

Useful checks in the container:

```bash
cat /tmp/omnifs.log
ssh -F /dev/null -T git@github.com
```

## Logging and debugging

- Runtime log file is `/tmp/omnifs.log` inside the container.
- Clone failures should surface there with `git clone` stderr.
- FUSE `access(...)` warnings are expected noise unless they correlate with a real failure.
- Use `omnifs status` inside the container for fast mount/config/plugin/cache triage.
- Do not assume `docker exec` inherits the entrypoint environment; verify live runtime paths rather than inferring them from defaults.

When a repo path returns `Input/output error`, check:

1. `docker compose logs omnifs`
2. SSH auth inside the container
3. whether the mount is still present in `/proc/mounts`

When debugging hangs or slow paths, start with user-visible probes before theory:

1. `cd /github/<owner>`
2. `cat /dns/@google/<domain>/MX`
3. `tail -n 80 /tmp/omnifs.log`

## Shell expectations

The runtime image uses Ubuntu 25.10 and `zsh`.

Expected interactive shell behavior:

- `ls` is aliased to `ls --color=auto`
- `ll` is aliased to `ls -lrt`

If changing shell behavior, prefer putting it in the image rather than generating per-session shell config.

## Build and test

Rust validation:

```bash
cargo fmt
cargo test
```

Docker build:

```bash
docker compose build
```

For mount/provider/clone changes, do not stop at Rust-only checks. Validate through the supported runtime path:

```bash
just dev
docker exec omnifs /bin/zsh -lc 'omnifs status'
docker exec omnifs /bin/zsh -lc 'OMNIFS_DEMO_MODE=smoke /tmp/demo.sh'
docker exec omnifs /bin/zsh -lc 'tail -n 80 /tmp/omnifs.log'
```

The Dockerfile is intentionally cache-oriented:

- multi-stage build
- `cargo-chef`
- BuildKit cache mounts
- minimal build context via `.dockerignore`

Preserve that structure unless there is a clear regression or simplification with equal caching behavior.

## Codebase expectations

- Keep changes small and local.
- Prefer preserving the current architecture:
  - inode table
  - router
  - providers
  - GitHub cache/scheduler/poller
  - clone manager
- Do not silently change the auth model or transport model.
- If switching clone transport from SSH to HTTPS/token, call that out explicitly because it changes the operational contract.
- When a refactor touches clone, routing, or traversal behavior, compare against the pre-refactor behavior before accepting the new result.
- Preserve existing repo-tree passthrough and ownership semantics unless intentionally changing the contract.
- Providers must project all data they have already fetched. If a handler has an upstream payload in hand, emit every sibling file and child that can be derived from it instead of returning only the requested field and forcing later refetches.

## Design judgment

- Prefer the simpler end-to-end flow, not the purer local abstraction.
- Bias toward single-phase designs over multi-phase orchestration on the hot path.
- Keep data near the point where it is naturally produced and immediately consumed; split it into a second mechanism only when that separation buys something concrete.
- Do not defend abstraction boundaries that add complexity in the common case.
- Once the direct path exists, remove bridge-style dispatch layers and other transitional glue instead of letting them harden into architecture.

## Design invariants

### Bash tool compatibility

omnifs paths must behave like real files for the standard Linux toolbox. Every code path is judged against this list. A change that makes any of these regress is wrong.

- **Read content**: `cat`, `head`, `tail` (incl. `-f`, `-n`, `-c`), `less`, `more`, `xxd`, `hexdump`, `od`, `file`.
- **Search and traversal**: `grep` (incl. `-r`), `rg`, `find` (incl. `-name`, `-size`, `-type`), `fd`.
- **Stat-based**: `ls` (incl. `-l`, `-h`), `du` (incl. `-sh`), `wc` (incl. `-l`, `-c`, `-m`), `stat`.
- **Copy and archive**: `cp`, `mv`, `tar` (`c`, `x`, `t`), `rsync`.
- **Compare and hash**: `diff`, `cmp`, `md5sum`, `sha256sum`, `b3sum`.
- **Inspection**: `jq`, `yq`, `xmllint`.
- **Editors**: `vim`, `neovim`, `nano`. Editors that mmap (some `code` configurations) are best-effort but should not break.

When introducing a new feature, the burden of proof is on the change to demonstrate (via the smoke harness in `tests/smoke/` or a unit test) that it does not regress any of the above.

### File attributes

Projected files declare `size`, `access`, and `mutability` attributes via the SDK's `Projection` API; the host wires `st_size`, FUSE flags, cache layers, and post-read invalidation from those attributes. Full design at `docs/design/file-attributes.md` — enum definitions, the single structural rule (`Volatile` ⇒ `Ranged`), the seven valid combinations, and the byte-source ↔ handler pairing.

## Protocol and contract guardrails

- Reuse source-of-truth terms. Do not invent new names for public surfaces unless the rename is explicit.
- Keep public contracts at the right layer. Host internals must not leak into SDK/WIT naming or semantics.
- Do not reuse an existing abstraction if it changes the behavior model. Semantic fit matters more than code reuse.
- For protocol changes, write the exact interaction trace first and reject extra hops on hot paths.
- If something is conceptually one-way, stop before making it `await`-shaped. Fix the boundary instead of forcing it through request/response machinery.

## Mutation protocol

Mutations are not implemented yet.

If adding them, prefer:

- read model remains read-only
- drafts live under a draft namespace
- execution is triggered by moving a prepared transaction directory into a control namespace

Do not make projected issue/PR files directly writable as an implicit mutation mechanism.
