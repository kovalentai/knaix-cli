#!/usr/bin/env bash
# Propose the next version from the commits a release branch carries.
#
#   scripts/propose-version.sh [base]     # base defaults to origin/main
#
# Prints the version and nothing else. The rule is RELEASING.md's, not strict
# semver: a breaking change takes the minor, everything else takes the patch.
#
# It reads commit subjects, never the diff, so an unmarked break reads as a
# patch and a headline feature reads as a patch. Hence a proposal the workflow
# lets you override.
#
# Exit codes:
#   0  a version was proposed
#   1  something is wrong (no commits, unreadable Cargo.toml)
set -euo pipefail

BASE="${1:-origin/main}"

cd "$(dirname "$0")/.."

if ! git rev-parse --verify --quiet "$BASE" >/dev/null; then
  echo "error: '$BASE' is not a ref this checkout knows" >&2
  exit 1
fi

CURRENT="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
if ! printf '%s' "$CURRENT" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
  echo "error: Cargo.toml does not state a semver version (read: '${CURRENT}')" >&2
  exit 1
fi

MAJOR="${CURRENT%%.*}"
REST="${CURRENT#*.}"
MINOR="${REST%%.*}"
PATCH="${REST#*.}"

# Only the commits this branch adds. A merge-base range rather than a plain
# two-dot diff, so a release branch that has had main merged back into it does
# not count main's own commits as its own.
RANGE="${BASE}...HEAD"

if [ -z "$(git log --format=%H "$RANGE")" ]; then
  echo "error: no commits between ${BASE} and HEAD; nothing to release" >&2
  exit 1
fi

# A `!` before the colon, allowing for a scope: feat!: / feat(chat)!:
BREAKING="$(git log --format='%s' "$RANGE" | grep -Ec '^[a-z]+(\([^)]*\))?!:' || true)"

# The footer form. %B is the whole message, so this catches a body that declares
# the break without the subject marking it.
if [ "$BREAKING" -eq 0 ]; then
  BREAKING="$(git log --format='%B' "$RANGE" | grep -Ec '^BREAKING[ -]CHANGE:' || true)"
fi

if [ "$BREAKING" -gt 0 ]; then
  printf '%s.%s.0\n' "$MAJOR" "$((MINOR + 1))"
else
  printf '%s.%s.%s\n' "$MAJOR" "$MINOR" "$((PATCH + 1))"
fi
