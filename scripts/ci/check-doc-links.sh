#!/usr/bin/env bash
# Fail if any tracked docs/ markdown file links to or names a docs path that does not exist.
#
# This catches the cheap, self-maintaining slice of documentation drift: dangling
# doc-to-doc references (deleted or renamed docs). It checks markdown link targets
# ending in .md/.html and inline `docs/...` path mentions. Code-symbol and code-path
# drift is intentionally out of scope; that is covered by the AGENTS.md "Documentation"
# doctrine (cite code, do not transcribe it), not by this lint.
set -euo pipefail
cd "$(dirname "$0")/../.."

report=""
while IFS= read -r -d '' f; do
  dir=$(dirname "$f")
  # Candidate targets: markdown link destinations, plus inline repo-root docs paths.
  targets=$(
    {
      grep -oE '\]\([^)]+\)' "$f" | sed -E 's/^\]\(//; s/\)$//'
      grep -oE 'docs/[A-Za-z0-9._/-]+\.(md|html)' "$f"
    } || true
  )
  while IFS= read -r raw; do
    [ -z "$raw" ] && continue
    t="${raw%%#*}"              # drop #anchor
    t="${t%%[[:space:]]*}"      # drop optional "title" and trailing space
    [ -z "$t" ] && continue
    case "$t" in
      http://* | https://* | mailto:*) continue ;;  # external
    esac
    case "$t" in
      docs/*) p="$t" ;;          # repo-root docs path
      /*) p=".${t}" ;;           # repo-absolute
      *.md | *.html) p="$dir/$t" ;;  # relative doc link
      *) continue ;;             # not a doc reference we check
    esac
    [ -e "$p" ] || report+="  DANGLING  $f  ->  $raw"$'\n'
  done <<< "$targets"
done < <(git ls-files -z -- 'docs/*.md')

if [ -n "$report" ]; then
  echo "Doc link check FAILED: docs reference nonexistent docs paths."
  printf '%s' "$report"
  echo "Fix the reference or repoint it; see the Documentation section in AGENTS.md."
  exit 1
fi
echo "Doc link check passed: no dangling docs references."
