---
title: Working with GitHub
description: Browse repos, issues, pull requests, and CI runs under /github. Repos are cloned on demand over SSH.
---

The GitHub provider mounts under `/github` and projects repositories, issues,
pull requests, and CI runs as directories and files. Read access uses a
read-only OAuth token; repository working trees are cloned on demand over SSH.

```bash
ls /github/torvalds          # repos under an owner
cd /github/ollama/ollama
ls                           # actions  issues  pulls  repo
```

## Path map

| Path | Content |
|------|---------|
| `/github/{owner}` | List repos for a user or org |
| `/github/{owner}/{repo}/repo/` | Cloned working tree (cloned on demand via SSH) |
| `/github/{owner}/{repo}/issues/open/` | List open issues |
| `/github/{owner}/{repo}/issues/all/` | List all issues |
| `/github/{owner}/{repo}/issues/{filter}/{n}/title` | Issue title |
| `/github/{owner}/{repo}/issues/{filter}/{n}/body` | Issue body (Markdown) |
| `/github/{owner}/{repo}/issues/{filter}/{n}/state` | Issue state |
| `/github/{owner}/{repo}/issues/{filter}/{n}/comments/{i}` | An individual comment |
| `/github/{owner}/{repo}/pulls/{filter}/{n}/diff` | PR diff |
| `/github/{owner}/{repo}/actions/runs/{id}/status` | CI run status |
| `/github/{owner}/{repo}/actions/runs/{id}/log` | CI run log |

`{filter}` is `open` or `all`.

## Listing repositories and entering a repo

`ls /github/{owner}` lists the owner's repositories. Each repo directory holds
the `repo` working tree plus the `issues`, `pulls`, and `actions` views.

```bash
ls /github/torvalds
cd /github/ollama/ollama
ls                           # actions  issues  pulls  repo
```

## Clone-on-list of `repo/`

The `repo/` directory is the actual working tree, cloned on demand the first
time you list it. omnifs clones over SSH using your forwarded SSH agent (see the
caution below). Once cloned, reads come from the local clone and are cached.

```bash
cd /github/ollama/ollama/repo
ls
cat README.md
grep -rn "func main" .
```

## Reading issues

Issues live under `issues/open/` and `issues/all/` as numbered directories. Each
issue exposes `title`, `body`, `state`, and a `comments/` directory.

```console
$ ls /github/ollama/ollama/issues/open
10333  10412  10455  10477  10489

$ cat /github/ollama/ollama/issues/open/10333/title
Allow setting a custom models directory per request

$ cat /github/ollama/ollama/issues/open/10333/state
open

$ ls /github/ollama/ollama/issues/open/10333/comments
0  1  2
```

## Pull request diffs

Pull requests follow the same `open`/`all` filter shape. The `diff` file is the
full PR diff.

```bash
cat /github/ollama/ollama/pulls/open/4242/diff
```

## CI run status and logs

CI runs live under `actions/runs/{id}/`. Read `status` for the outcome and `log`
for the full output; `tail -f` follows a running log.

```bash
cat    /github/ollama/ollama/actions/runs/123456/status
cat    /github/ollama/ollama/actions/runs/123456/log
tail -f /github/ollama/ollama/actions/runs/123456/log
```

## Auth and transport

- **Read access** uses a read-only GitHub OAuth token (device-code flow, no
  default scopes). Authenticate with `omnifs init github` or
  `omnifs auth login github`, or import a personal access token with
  `omnifs auth import`. See [Authentication](/guides/authentication/).
- **Cloning** uses SSH (`git@github.com:{owner}/{repo}.git`) and your forwarded
  SSH agent. omnifs does not copy private keys into the container.

:::caution
If a repo path returns `Input/output error`, the clone most likely failed.
Verify your SSH agent has a GitHub key (`ssh-add -L`) and that
`ssh -T git@github.com` succeeds on your host — it must work on the host for it
to work in the container. See [Troubleshooting](/guides/troubleshooting/).
:::
