set shell := ["bash", "-euo", "pipefail", "-c"]
set working-directory := '..'

# Validate release metadata for a release PR.
check:
    bun scripts/release.ts check

# Emit the JSON release plan used by automation.
plan:
    @bun scripts/release.ts plan --format json

# Cut a release branch, push it, and open the release PR.
[confirm("Cut a release branch, push it, and open a PR?")]
cut *args:
    bun scripts/release.ts cut {{ args }}

# Cut a release branch locally without pushing.
[confirm("Cut a local release branch without pushing?")]
cut-local *args:
    bun scripts/release.ts cut --no-push {{ args }}
