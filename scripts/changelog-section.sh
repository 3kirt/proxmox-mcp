#!/bin/sh
# Print the CHANGELOG.md body for version $1: the lines between its
# "## [X.Y.Z] - date" heading and the next "## [" heading. Exits non-zero when
# the section is missing or empty. Used by release.yml (verify + notes).
set -eu
ver="${1:?usage: changelog-section.sh <version>}"
file="${CHANGELOG:-$(dirname "$0")/../CHANGELOG.md}"
body=$(awk -v h="## [$ver]" '
  index($0, h) == 1 { on = 1; next }
  on && /^## \[/ { exit }
  on { print }
' "$file")
if [ -z "$(printf '%s' "$body" | tr -d '[:space:]')" ]; then
  echo "no CHANGELOG section for $ver in $file" >&2
  exit 1
fi
printf '%s\n' "$body" | sed -e '/./,$!d'
