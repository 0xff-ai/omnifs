# Auth and cloning

**Read when:** changing auth flow, credential injection, secret handling, git
remote/clone behavior, or anything that touches `GITHUB_TOKEN` /
`SSH_AUTH_SOCK`. Also read before suggesting a transport change (SSH ↔
HTTPS).

**Update when:** adding a new credential source, changing how tokens reach
the host or providers, switching git clone transport (SSH ↔ HTTPS/token),
adding a new auth-related provider capability, or changing the operational
contract for required host setup.

## GitHub API auth

Two intake mechanisms, both supported by the baked provider config:

- `token_file` — used by the Docker Compose path. Host writes the token to
  `.secrets/github_token`; Compose mounts it at `/run/secrets/github_token`.
- `token_env` — used by `just start`, which passes `GITHUB_TOKEN` directly.

Don't silently switch between these. If you change which one is used, call
that out in the PR.

## Git clone transport

Git clones currently use **SSH**:

- Remote format: `git@github.com:<owner>/<repo>.git`
- Auth: forwarded `SSH_AUTH_SOCK`
- Do not mount host private keys into the container.

If you switch clone transport from SSH to HTTPS/token, **call that out
explicitly** — it changes the operational contract.

## Required host setup for the container

- `gh auth token` works (so `.secrets/github_token` can be created), or
  `GITHUB_TOKEN` is set if using `just start`.
- `SSH_AUTH_SOCK` is set on the host.
- Host SSH agent has a usable GitHub key loaded.

## Sanity checks

On the host:

```bash
test -s .secrets/github_token || gh auth token > .secrets/github_token
ssh-add -L
ssh -T git@github.com
```

In the container:

```bash
cat /tmp/omnifs.log
ssh -F /dev/null -T git@github.com
```
