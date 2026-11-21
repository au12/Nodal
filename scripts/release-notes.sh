#!/usr/bin/env bash
# Prints the GitHub release notes for a tag: its CHANGELOG.md section followed by
# .github/release-template.md (downloads and install notes).
#   scripts/release-notes.sh v0.1.0
set -euo pipefail
cd "$(dirname "$0")/.."
TAG=${1:?usage: release-notes.sh vX.Y.Z}
VERSION=${TAG#v}
MINOR=$(echo "$VERSION" | cut -d. -f1-2)

SECTION=$(awk -v v="$VERSION" '
  /^## \[/ { if (found) exit; if (index($0, "## [" v "]") == 1) { found = 1; next } }
  /^\[[^]]+\]: / { if (found) exit }
  found { print }
' CHANGELOG.md)
if [ -z "$(printf '%s' "$SECTION" | tr -d '[:space:]')" ]; then
  echo "warning: CHANGELOG.md has no section for $VERSION" >&2
  SECTION="Nodal $VERSION"
fi

printf '%s\n\n' "$SECTION" | sed -e '/./,$!d'
sed -e "s/{{VERSION}}/$VERSION/g" -e "s/{{MINOR}}/$MINOR/g" -e "s/{{TAG}}/$TAG/g" .github/release-template.md
