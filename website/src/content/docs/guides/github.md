---
title: Working with GitHub
description: Browse repos, issues, pull requests, and CI runs under /github. Repos are cloned on demand over SSH.
---

The GitHub provider mounts under `/github` and projects repositories, issues,
pull requests, and CI runs as directories and files. Read access uses a
read-only OAuth token; repository working trees are cloned on demand over SSH.

```bash
ls /github/torvalds          # repos under an owner
cd /github/torvalds/linux
cat README
```

## Path map

| Path | Content |
|------|---------|
| `/github/<owner>` | List of the owner's repositories |
| `/github/<owner>/<repo>/repo` | Cloned working tree (files, directories) |
| `/github/<owner>/<repo>/issues` | Open issues as numbered directories |
| `/github/<owner>/<repo>/issues/<n>/body` | Issue body (Markdown) |
| `/github/<owner>/<repo>/issues/<n>/comments` | Issue comments |
| `/github/<owner>/<repo>/pulls` | Open pull requests |
| `/github/<owner>/<repo>/pulls/<n>/diff` | PR diff |
| `/github/<owner>/<repo>/pulls/<n>/files` | Files changed in the PR |
| `/github/<owner>/<repo>/actions` | CI runs |
| `/github/<owner>/<repo>/actions/<run>/log` | Run log |

## Listing repositories and entering a repo

`ls /github/<owner>` lists the owner's repositories. Each repo directory holds
the `repo` working tree plus the `issues`, `pulls`, and `actions` views.

```bash
ls /github/rust-lang
cd /github/rust-lang/rust
ls                           # repo  issues  pulls  actions
```

## Clone-on-list of `repo/`

The `repo/` directory is the actual working tree, cloned on demand the first
time you navigate into it. omnifs clones over SSH using your forwarded SSH agent
(see the caution below). Once cloned, reads come from the local clone and are
cached.

```bash
cd /github/torvalds/linux/repo
ls kernel/
cat README
grep -rn "EXPORT_SYMBOL" kernel/
```

:::note
Large repos take time to clone on first access. The clone runs in the
background; the path becomes available once it completes. Subsequent reads are
fast.
:::

## Reading open issues

Issues appear as numbered directories. The `body` file is the issue text;
`comments` holds the discussion.

```bash
ls /github/rust-lang/rust/issues
cat /github/rust-lang/rust/issues/100000/body
cat /github/rust-lang/rust/issues/100000/comments
```

## Pull request diffs and changed files

```bash
ls /github/rust-lang/rust/pulls
cat /github/rust-lang/rust/pulls/50000/diff
ls  /github/rust-lang/rust/pulls/50000/files
```

## CI run status and logs

The `actions` directory lists CI runs. Each run exposes a `log` file you can
read or follow.

```bash
ls /github/myorg/myrepo/actions
cat  /github/myorg/myrepo/actions/<run>/log
tail -f /github/myorg/myrepo/actions/<run>/log
```

## Auth and transport

- **Read access** uses a read-only GitHub OAuth token. Authenticate with
  `omnifs auth login github` or import a personal access token with
  `omnifs auth import`. See [Authentication](/guides/authentication/).
- **Cloning** uses SSH (`git@github.com:<owner>/<repo>.git`) and your forwarded
  SSH agent. omnifs does not mount private keys into the container.

:::caution
If a repo path returns `Input/output error`, the clone most likely failed.
Verify your SSH agent has a GitHub key (`ssh-add -L`) and that
`ssh -T git@github.com` succeeds on your host — it must work on the host for it
to work in the container. See [Troubleshooting](/guides/troubleshooting/).
:::
