# Validate release metadata for a release PR.
[group('release')]
release-check:
    bun scripts/release.ts check

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
