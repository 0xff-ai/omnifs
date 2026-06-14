# Draft changelog notes from commits since the last release tag.
[group('release')]
release-prompt:
    @bun scripts/release.ts prompt

# Draft a PR's changelog entry (3 lengths) into a sticky comment (LLM; needs OPENCODE_ZEN_API_KEY).
[group('release')]
release-notes-draft pr:
    bun scripts/release-notes.ts draft --pr '{{ pr }}'

# Record the chosen changelog entry for HEAD into CHANGELOG.md [Unreleased].
[group('release')]
release-notes-record:
    bun scripts/release-notes.ts record

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
