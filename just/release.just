# Draft changelog notes from commits since the last release tag.
[group('release')]
release-prompt:
    @bun scripts/release.ts prompt

# Draft an editable release-note comment for a PR (LLM; needs ANTHROPIC_API_KEY).
# Pass --force to overwrite an existing note comment.
[group('release')]
release-notes-draft pr *args:
    bun scripts/release-notes.ts draft --pr '{{ pr }}' {{ args }}

# Validate that a PR carries a release note (or the no-changelog label).
[group('release')]
release-notes-check pr:
    bun scripts/release-notes.ts check --pr '{{ pr }}'

# Fold merged PR notes into the standing release/vX.Y.Z PR.
[group('release')]
release-pr-maintain:
    bun scripts/release-notes.ts maintain

# Validate release metadata for a PR or branch.
[group('release')]
release-check base='origin/main' head='HEAD':
    bun scripts/release.ts check --base '{{ base }}' --head '{{ head }}'

# Emit the text release plan used by the Release workflow.
[group('release')]
release-plan:
    @bun scripts/release.ts plan

# Emit the JSON release plan used by automation.
[group('release')]
release-plan-json:
    @bun scripts/release.ts plan --format json

# Cut a release branch, push it, and open the release PR.
[confirm("Cut a release branch, push it, and open a PR?")]
[group('release')]
release-cut *args:
    bun scripts/release.ts cut {{ args }}

# Cut a release branch locally without pushing.
[confirm("Cut a local release branch without pushing?")]
[group('release')]
release-cut-local *args:
    bun scripts/release.ts cut --no-push {{ args }}
