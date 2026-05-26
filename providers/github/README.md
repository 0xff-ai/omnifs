# omnifs-provider-github

[omnifs](https://github.com/0xff-ai/omnifs) provider that projects GitHub repositories, issues, pull requests, and CI runs into a FUSE-visible tree. Source trees are bind-mounted clones (cloned on demand via SSH); issues and PRs are per-item directories with title, body, state, and comments as separate files.

## Mount layout

```
/github/{owner}/{repo}/
  _issues/{open|all}/{number}/
    title
    body
    state
    user
    comments/{n}
  _prs/{open|all}/{number}/
    title
    body
    state
    user
    diff
    comments/{n}
  _actions/runs/{id}/
    status
    conclusion
    log
  _repo/  ← bind-mounted clone, lazily cloned via SSH
```

Hybrid pagination across issues + PRs and cross-listing PR projection keep listings responsive without exhausting GitHub's API budget.

## Capabilities

`api.github.com` over HTTPS plus `git@github.com:*` over SSH for the bind-mounted clones. The default user workflow is `omnifs init github`, which runs GitHub's device flow with product client id `Ov23licogxMDzS47s9sF` and no default scopes. That default is public-read only; use `omnifs init github --scope repo` only when you need private repository access and accept GitHub OAuth's broad private-repository grant. Bearer-token auth via `GITHUB_TOKEN` env or a Docker secret file remains supported for the development Compose path. 256 MiB memory limit. Read-only today; mutation path WIP per the design docs.

For OAuth setup, see `docs/oauth.md`.

## Install

This is a wasm component. Build with:

```bash
cargo build --target wasm32-wasip2 --release -p omnifs-provider-github
```

The resulting `omnifs_provider_github.wasm` is also attached to each [GitHub Release](https://github.com/0xff-ai/omnifs/releases). The provider's `omnifs.provider.json` is the source for the embedded provider metadata and auth section, and drives `omnifs init github`.

## Status

Pre-1.0. Mount layout and projection rules may evolve.

## License

Dual licensed under MIT or Apache-2.0 at your option.
