#!/usr/bin/env bash
# Draft a CHANGELOG.md section from the commits a release branch carries.
#
#   scripts/draft-changelog.sh 0.5.4 [base]    # base defaults to origin/main
#
# Inserts the section above the newest existing one.
#
# What comes out is a skeleton: pull request titles say what changed and never
# why it mattered, which is what a changelog is read for. Rewrite every line.
# Generating it keeps things from being forgotten, not from being written.
#
# chore, ci, docs, test and build are left out; the workflow lists them
# separately so a user-facing change filed under the wrong prefix can be found.
#
# Exit codes:
#   0  the section was inserted
#   1  something is wrong (bad argument, the version is already in the file)
set -euo pipefail

VERSION="${1:-}"
BASE="${2:-origin/main}"

if [ -z "$VERSION" ]; then
  echo "usage: scripts/draft-changelog.sh <version> [base]   e.g. 0.5.4" >&2
  exit 1
fi

if ! printf '%s' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
  echo "error: '$VERSION' is not a semver version like 0.5.4" >&2
  exit 1
fi

cd "$(dirname "$0")/.."

if grep -qF "## [${VERSION}]" CHANGELOG.md; then
  echo "error: CHANGELOG.md already has a section for ${VERSION}" >&2
  exit 1
fi

RANGE="${BASE}...HEAD"

# Strip the conventional prefix and the trailing PR number, keeping the scope.
entries() {
  local pattern="$1"
  # One pass with a branch, not two: in two, the unscoped rule matches the
  # scoped rule's output and strips the scope it just kept.
  git log --reverse --format='%s' "$RANGE" \
    | grep -E "^(${pattern})(\([^)]*\))?!?:" \
    | sed -E -e 's/^[a-z]+\(([^)]*)\)!?: */\1: /; t' -e 's/^[a-z]+!?: *//' \
    | sed -E 's/ *\(#[0-9]+\) *$//' \
    | sed -E 's/^/- **/; s/$/**/'
}

section() {
  local heading="$1" pattern="$2" body
  body="$(entries "$pattern")"
  [ -z "$body" ] && return 0
  printf '### %s\n\n%s\n\n' "$heading" "$body"
}

DRAFT="$(
  printf '## [%s] - %s\n\n' "$VERSION" "$(date -u +%Y-%m-%d)"
  printf '<!-- One paragraph on what this release is about. Delete this comment. -->\n\n'
  section Added   'feat'
  section Changed 'perf|refactor|change'
  section Fixed   'fix'
)"

# Split and reassemble rather than edit in place: sed -i differs between GNU
# and BSD, and BSD awk will not take a multi-line value through -v.
FIRST="$(grep -n '^## \[' CHANGELOG.md | head -1 | cut -d: -f1)"

tmp="$(mktemp)"
if [ -n "$FIRST" ]; then
  head -n "$((FIRST - 1))" CHANGELOG.md > "$tmp"
  # Two newlines: command substitution stripped the draft's trailing blank one.
  printf '%s\n\n' "$DRAFT" >> "$tmp"
  tail -n "+${FIRST}" CHANGELOG.md >> "$tmp"
else
  # No sections yet, so the draft is simply appended.
  cp CHANGELOG.md "$tmp"
  printf '\n%s\n' "$DRAFT" >> "$tmp"
fi
mv "$tmp" CHANGELOG.md

if ! grep -qF "## [${VERSION}]" CHANGELOG.md; then
  echo "error: the section was not inserted" >&2
  exit 1
fi

echo "Drafted the ${VERSION} section in CHANGELOG.md. Now rewrite it."
