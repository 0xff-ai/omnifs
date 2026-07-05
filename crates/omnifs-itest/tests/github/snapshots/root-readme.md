# Omnifs route schema

This README is generated from the sealed provider route table for `/`.

## Keying schema

The keying schema is the path grammar below. Literal segments are written as-is. Captures such as `{owner}` are parsed by the provider SDK. A finite choice list means only those path values are valid. Lookup may resolve capture values that `ls` cannot enumerate.

## Route templates

- `/{owner}/{repo}/issues` - directory
  - `{owner}`: `String`
  - `{repo}`: `String`
- `/{owner}/{repo}/pulls` - directory
  - `{owner}`: `String`
  - `{repo}`: `String`
- `/notifications` - directory
- `/{owner}/{repo}/repo` - subtree
  - `{owner}`: `OwnerName`
  - `{repo}`: `RepoName`
- `/{owner}` - object `github.owner`
  - `{owner}`: `OwnerName`
- `/{owner}/{repo}` - object `github.repo`
  - `{owner}`: `OwnerName`
  - `{repo}`: `RepoName`
- `/{owner}/{repo}/issues/{filter}/{number}` - object `github.issue`
  - `{owner}`: `OwnerName`
  - `{repo}`: `RepoName`
  - `{filter}`: `StateFilter` choices `open`, `all`
  - `{number}`: `u64`
- `/{owner}/{repo}/pulls/{filter}/{number}` - object `github.pull`
  - `{owner}`: `OwnerName`
  - `{repo}`: `RepoName`
  - `{filter}`: `StateFilter` choices `open`, `all`
  - `{number}`: `u64`
- `/{owner}/{repo}/pulls/{filter}/{number}/files/{path}` - object `github.pull_file`
  - `{owner}`: `OwnerName`
  - `{repo}`: `RepoName`
  - `{filter}`: `StateFilter` choices `open`, `all`
  - `{number}`: `u64`
  - `{path}`: `FilePath`
- `/{owner}/{repo}/pulls/{filter}/{number}/reviews/{review_id}` - object `github.review`
  - `{owner}`: `OwnerName`
  - `{repo}`: `RepoName`
  - `{filter}`: `StateFilter` choices `open`, `all`
  - `{number}`: `u64`
  - `{review_id}`: `u64`
- `/{owner}/{repo}/pulls/{filter}/{number}/reviews/{review_id}/comments/{comment_id}` - object `github.review_comment`
  - `{owner}`: `OwnerName`
  - `{repo}`: `RepoName`
  - `{filter}`: `StateFilter` choices `open`, `all`
  - `{number}`: `u64`
  - `{review_id}`: `u64`
  - `{comment_id}`: `u64`
- `/{owner}/{repo}/pulls/{filter}/{number}/checks/{check_run_id}` - object `github.check`
  - `{owner}`: `OwnerName`
  - `{repo}`: `RepoName`
  - `{filter}`: `StateFilter` choices `open`, `all`
  - `{number}`: `u64`
  - `{check_run_id}`: `u64`
- `/{owner}/{repo}/actions/runs/{run_id}` - object `github.run`
  - `{owner}`: `OwnerName`
  - `{repo}`: `RepoName`
  - `{run_id}`: `u64`
- `/{owner}/{repo}/{item_kind}/{filter}/{number}/comments/{comment_id}` - object `github.comment`
  - `{owner}`: `OwnerName`
  - `{repo}`: `RepoName`
  - `{item_kind}`: `ItemKind`
  - `{filter}`: `StateFilter` choices `open`, `all`
  - `{number}`: `u64`
  - `{comment_id}`: `u64`
- `/notifications/thread-{thread_id}` - object `github.notification`
  - `{thread_id}`: `ThreadId`
- `/{owner}/{repo}` - collection of `github.repo`
  - `{owner}`: `OwnerName`
  - `{repo}`: `String`
- `/{owner}/{repo}/issues/{filter}` - collection of `github.issue`
  - `{owner}`: `OwnerName`
  - `{repo}`: `RepoName`
  - `{filter}`: `StateFilter`
- `/{owner}/{repo}/pulls/{filter}` - collection of `github.pull`
  - `{owner}`: `OwnerName`
  - `{repo}`: `RepoName`
  - `{filter}`: `StateFilter`
- `/{owner}/{repo}/actions/runs` - collection of `github.run`
  - `{owner}`: `OwnerName`
  - `{repo}`: `RepoName`
- `/{owner}/{repo}/issues/{filter}/{number}/comments` - collection of `github.comment`
  - `{owner}`: `OwnerName`
  - `{repo}`: `RepoName`
  - `{filter}`: `StateFilter` choices `open`, `all`
  - `{number}`: `u64`
- `/{owner}/{repo}/pulls/{filter}/{number}/comments` - collection of `github.comment`
  - `{owner}`: `OwnerName`
  - `{repo}`: `RepoName`
  - `{filter}`: `StateFilter` choices `open`, `all`
  - `{number}`: `u64`
- `/{owner}/{repo}/pulls/{filter}/{number}/files` - collection of `github.pull_file`
  - `{owner}`: `OwnerName`
  - `{repo}`: `RepoName`
  - `{filter}`: `StateFilter` choices `open`, `all`
  - `{number}`: `u64`
- `/{owner}/{repo}/pulls/{filter}/{number}/reviews` - collection of `github.review`
  - `{owner}`: `OwnerName`
  - `{repo}`: `RepoName`
  - `{filter}`: `StateFilter` choices `open`, `all`
  - `{number}`: `u64`
- `/{owner}/{repo}/pulls/{filter}/{number}/checks` - collection of `github.check`
  - `{owner}`: `OwnerName`
  - `{repo}`: `RepoName`
  - `{filter}`: `StateFilter` choices `open`, `all`
  - `{number}`: `u64`
- `/{owner}/{repo}/pulls/{filter}/{number}/reviews/{review_id}/comments` - collection of `github.review_comment`
  - `{owner}`: `OwnerName`
  - `{repo}`: `RepoName`
  - `{filter}`: `StateFilter` choices `open`, `all`
  - `{number}`: `u64`
  - `{review_id}`: `u64`

## Example commands

- `ls .`
- `ls './{owner}/{repo}/issues'`
- `cat './{owner}/owner.json'`

## Bulk traversal

Mount-root ignore files hide generated README leaves and pagination controls from ignore-respecting recursive tools. Read this file explicitly with `cat README.md` when you need the schema.
