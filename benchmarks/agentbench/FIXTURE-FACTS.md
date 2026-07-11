# Fixture facts (answer key)

The planted facts that the `tasks/*.yaml` prompts ask about. This file lives outside `fixture-data/` on purpose: an agent exploring the corpus must not be able to read the answers. Regenerate the corpus with `bun gen-fixture.ts`; these facts are stable across regenerations.

| task | family | fact | answer |
|---|---|---|---|
| corr-001 | correlation | `github/repos/acme-api/issues/3/body.md` tracks Linear `ENG-107`; `linear/issues/ENG-107/issue.md` assignee | Priya Rao |
| corr-002 | correlation | `linear/issues/OPS-204/issue.md` references a GitHub issue | acme-web#7 |
| nav-001 | navigation | title of `github/repos/acme-web/issues/5/body.md` | Guard against duplicate reconcile runs |
| nav-002 | navigation | label on `github/repos/acme-cli/issues/2/body.md` | enhancement |
| agg-001 | aggregation | closed issues in `acme-api` | 4 |
| agg-002 | aggregation | total comment files across `acme-cli` issues | 9 |
| recon-001 | reconstruction | status before `Done` in `linear/issues/ENG-115/activity.md` | In Review |
| recon-002 | reconstruction | config value proposed in `github/repos/acme-api/issues/3/comments/` | max_retries=5 |
| judge-001 | navigation | theme of `acme-api#3` (model-based grader not implemented) | retry backoff; tracked as ENG-107 |

The aggregation family (agg-001, agg-002) is where a query tool or SQL should beat a filesystem. It is included deliberately so the report shows where files win and where they do not.
