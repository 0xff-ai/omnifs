---
title: "Path schemes"
description: "Copy-paste path reference for the live omnifs providers."
---

Paths below are relative to the runtime mount root. Inside `omnifs shell`, prefix them with `/omnifs`. Braces mark placeholders; alternatives inside braces are notation, not literal path text.

## GitHub

Mount: `/github`

| Operation | Path |
|---|---|
| List owner repos | `/github/{owner}` |
| Repo object | `/github/{owner}/{repo}` |
| Repo JSON | `/github/{owner}/{repo}/repo.json` |
| Repo tree handoff | `/github/{owner}/{repo}/repo` |
| Issue filters | `/github/{owner}/{repo}/issues` |
| Issue list | `/github/{owner}/{repo}/issues/{filter}` |
| Issue object | `/github/{owner}/{repo}/issues/{filter}/{number}` |
| Issue title/body/state/user | `/github/{owner}/{repo}/issues/{filter}/{number}/{title|body|state|user}` |
| Issue JSON/Markdown | `/github/{owner}/{repo}/issues/{filter}/{number}/{item.json|item.md}` |
| Issue comments | `/github/{owner}/{repo}/issues/{filter}/{number}/comments` |
| Issue comment | `/github/{owner}/{repo}/issues/{filter}/{number}/comments/{idx}` |
| Pull request list | `/github/{owner}/{repo}/pulls/{filter}` |
| Pull request object | `/github/{owner}/{repo}/pulls/{filter}/{number}` |
| Pull request title/body/state/user | `/github/{owner}/{repo}/pulls/{filter}/{number}/{title|body|state|user}` |
| Pull request JSON/Markdown | `/github/{owner}/{repo}/pulls/{filter}/{number}/{item.json|item.md}` |
| Pull request diff | `/github/{owner}/{repo}/pulls/{filter}/{number}/diff` |
| Pull request comments | `/github/{owner}/{repo}/pulls/{filter}/{number}/comments/{idx}` |
| Actions run | `/github/{owner}/{repo}/actions/runs/{run_id}` |
| Actions run leaves | `/github/{owner}/{repo}/actions/runs/{run_id}/{run.json|status|conclusion|log}` |

Comment indexes are 1-based.

## Docker

Mount: `/docker`

| Operation | Path |
|---|---|
| System info | `/docker/system/info.json` |
| Daemon version | `/docker/system/version.json` |
| Disk usage | `/docker/system/df.json` |
| Ping | `/docker/system/ping` |
| Container list JSON | `/docker/containers.json` |
| Compose list JSON | `/docker/compose.json` |
| Containers by name | `/docker/containers/by-name` |
| Containers by id | `/docker/containers/by-id` |
| Running containers | `/docker/containers/running` |
| Stopped containers | `/docker/containers/stopped` |
| Container inspect | `/docker/containers/{by-name|by-id|running|stopped}/{reference}/inspect.json` |
| Container state | `/docker/containers/{by-name|by-id|running|stopped}/{reference}/state` |
| Container summary | `/docker/containers/{by-name|by-id|running|stopped}/{reference}/summary.txt` |
| Compose project | `/docker/compose/{project}` |
| Compose services | `/docker/compose/{project}/services` |
| Compose service containers | `/docker/compose/{project}/services/{service}/containers` |
| Compose container leaves | `/docker/compose/{project}/services/{service}/containers/{reference}/{inspect.json|state|summary.txt}` |

## arXiv

Mount: `/arxiv`

| Operation | Path |
|---|---|
| Paper object | `/arxiv/papers/{id}` |
| Paper metadata | `/arxiv/papers/{id}/paper.json` |
| PDF | `/arxiv/papers/{id}/paper.pdf` |
| Source archive | `/arxiv/papers/{id}/source.tar.gz` |
| Versions | `/arxiv/papers/{id}/versions` |
| Version object | `/arxiv/papers/{id}/versions/v{n}` |
| Version metadata | `/arxiv/papers/{id}/versions/v{n}/paper.json` |
| Version PDF | `/arxiv/papers/{id}/versions/v{n}/paper.pdf` |
| Version source archive | `/arxiv/papers/{id}/versions/v{n}/source.tar.gz` |
| Category | `/arxiv/categories/{category}` |
| Category papers | `/arxiv/categories/{category}/papers` |

## Linear

Mount: `/linear`

| Operation | Path |
|---|---|
| Teams | `/linear/teams` |
| Team issues | `/linear/teams/{team}/issues` |
| Issue list | `/linear/teams/{team}/issues/{filter}` |
| Issue object | `/linear/teams/{team}/issues/{filter}/{ident}` |
| Issue JSON/Markdown | `/linear/teams/{team}/issues/{filter}/{ident}/{item.json|item.md}` |
| Title | `/linear/teams/{team}/issues/{filter}/{ident}/title` |
| State | `/linear/teams/{team}/issues/{filter}/{ident}/state` |
| Priority | `/linear/teams/{team}/issues/{filter}/{ident}/priority` |
| Assignee | `/linear/teams/{team}/issues/{filter}/{ident}/assignee` |
| Description | `/linear/teams/{team}/issues/{filter}/{ident}/description.md` |

## DNS

Mount: `/dns`

| Operation | Path |
|---|---|
| Resolvers | `/dns/resolvers` |
| Domain record types | `/dns/{domain}` |
| Record answer | `/dns/{domain}/{record}` |
| CAA answer | `/dns/{domain}/CAA` |
| All answers | `/dns/{domain}/all` |
| Raw resolver response | `/dns/{domain}/raw` |
| Reverse lookup | `/dns/reverse/{ip}` |
| Resolver root | `/dns/@{resolver}` |
| Resolver-scoped domain | `/dns/@{resolver}/{domain}` |
| Resolver-scoped record | `/dns/@{resolver}/{domain}/{record}` |
| Resolver-scoped reverse | `/dns/@{resolver}/reverse/{ip}` |

## db

Mount: `/db`

| Operation | Path |
|---|---|
| Metadata object | `/db/meta` |
| Metadata JSON | `/db/meta/info.json` |
| SQLite version | `/db/meta/version.txt` |
| Database path | `/db/meta/path.txt` |
| Tables | `/db/tables` |
| Table object | `/db/tables/{table}` |
| Table JSON | `/db/tables/{table}/table.json` |
| Schema SQL | `/db/tables/{table}/schema.sql` |
| Schema JSON | `/db/tables/{table}/schema.json` |
| Index metadata | `/db/tables/{table}/indexes.json` |
| Count | `/db/tables/{table}/count.txt` |
| Sample | `/db/tables/{table}/sample.json` |
