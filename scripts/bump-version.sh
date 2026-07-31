#!/usr/bin/env bash
# Set the CLI's version everywhere it is stated.
#
#   scripts/bump-version.sh 0.4.7
#
# The version appears in Cargo.toml and Cargo.lock and they have to agree:
# release.yml refuses a tag that disagrees with Cargo.toml, so a release cannot
# ship with a stale one. This is what a release PR runs instead of editing them
# by hand.
#
# The README does not state the version. It used to, in its title, and it went
# three releases stale before anyone noticed; it now carries a release badge
# that reads the latest tag, so there is nothing here to keep in step.
#
# CHANGELOG.md is left alone on purpose. Its heading carries a date and a
# section of prose somebody has to write, so a script that inserted one would
# only ever produce a heading to be rewritten.
#
# Exit codes:
#   0  every file now states the version
#   1  something is wrong (bad argument, a file did not change)
set -euo pipefail

VERSION="${1:-}"

if [ -z "$VERSION" ]; then
  echo "usage: scripts/bump-version.sh <version>   e.g. 0.4.7" >&2
  exit 1
fi

if ! printf '%s' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
  echo "error: '$VERSION' is not a semver version like 0.4.7" >&2
  exit 1
fi

cd "$(dirname "$0")/.."

# In place, portable between GNU and BSD sed, which differ on -i.
replace() {
  local pattern="$1" file="$2"
  local tmp
  tmp="$(mktemp)"
  sed -E "$pattern" "$file" > "$tmp"
  mv "$tmp" "$file"
}

replace "1,10s/^version = \"[0-9]+\.[0-9]+\.[0-9]+\"/version = \"${VERSION}\"/" Cargo.toml

# Cargo.lock states it too, and `cargo update -p` is the only thing that may
# edit that file. --offline so a version bump never depends on the network.
cargo update -p knaix --offline >/dev/null 2>&1 || cargo update -p knaix >/dev/null

# Verify rather than trust: a sed that matched nothing exits 0 and leaves the
# file stale, which is the exact failure this script exists to prevent.
fail=0
check() {
  local file="$1" expected="$2"
  if ! grep -qF "$expected" "$file"; then
    echo "error: ${file} does not state ${VERSION} (expected: ${expected})" >&2
    fail=1
  fi
}

check Cargo.toml    "version = \"${VERSION}\""
check Cargo.lock    "version = \"${VERSION}\""

if [ "$fail" -ne 0 ]; then
  exit 1
fi

echo "Set to ${VERSION}: Cargo.toml, Cargo.lock"
echo "Next: write the CHANGELOG.md section, open the release PR, then tag v${VERSION}."
