# omnifs-provider-github

[omnifs](https://github.com/0xff-ai/omnifs) provider that projects GitHub repositories, issues, pull requests, and CI runs into a filesystem tree. Source trees are bind-mounted clones (cloned on demand via SSH); issues and PRs are per-item directories with title, body, state, and comments as separate files.

## Mount layout

```
/github/{owner}/{repo}/
  issues/
    open|all/
      {number}/
        title
        body
        state
        user
        comments/{n}
  pulls/
    open|all/
      {number}/
        title
        body
        state
        user
        diff.patch
        comments/{n}
        files/{percent-encoded-path}/
        reviews/{id}/
        checks/{id}/
  actions/runs/{id}/
    status
    conclusion
    log
  repo/  ← bind-mounted clone, lazily cloned via SSH
/github/notifications/thread-{id}/
```

Issues and pulls use ordered REST page cursors. Issues filter PR-shaped rows after each fetch, while the raw page length decides whether another page exists.

## Capabilities

`api.github.com` over HTTPS plus `git@github.com:*` over SSH for the bind-mounted clones. The default user workflow is `omnifs mount add github`, which runs GitHub's device flow with product client id `Ov23licogxMDzS47s9sF` and no default scopes. That default is public-read only; use `omnifs mount add github --scope repo` only when you need private repository access and accept GitHub OAuth's broad private-repository grant. Use `omnifs mount reauth github` to repair a missing or expired credential after the mount exists, or `omnifs mount add github --token-env GITHUB_TOKEN` to authenticate with a personal access token from an environment variable. 256 MiB memory limit. Read-only today; mutation path WIP per the design docs.

## Install

This is a wasm component. Build with:

```bash
cargo build --target wasm32-wasip2 --release -p omnifs-provider-github
```

Release CLI binaries embed this provider and unpack it into `OMNIFS_HOME/providers`. Provider metadata and the auth section are authored from `#[omnifs_sdk::provider]` annotations and embedded in the wasm `omnifs.provider-metadata.v1` section at build time; `omnifs mount add github` reads that metadata from the wasm.

## Status

Pre-1.0. Mount layout and projection rules may evolve.

## License

Dual licensed under MIT or Apache-2.0 at your option.
