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
        comments/{id}/
          author
          body.md
  pulls/
    open|all/
      {number}/
        title
        body
        state
        user
        diff.patch
        comments/{id}/
          author
          body.md
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

The feed of a repository keeps its issues, pull requests, and comments. The first read of `issues/all/`, `issues/open/`, `pulls/all/`, or `pulls/open/` takes the first full listing a page at a time: every issue and pull request by `updated_at`, then every comment, then the GitHub GraphQL counts of the comments of each item; a stopped listing continues from its next page at the next read of those folders. After it, a read 3 seconds or more after the last answer checks the changes since the last `updated_at` with `If-None-Match`, the comments since then, and the GraphQL counts, a part of the answer for each request, which the host publishes together with the last part. Before the first full listing ends, an item answers from its own `GET /repos/{owner}/{repo}/issues/{number}`, and its `comments/` downloads at its first read for a count other than `0`. The facts of an issue or a pull request live under `all/`, which `open/` shares while it is open. GitHub GraphQL answers only an authenticated mount.

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
