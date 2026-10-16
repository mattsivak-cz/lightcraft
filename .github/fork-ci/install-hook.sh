#!/bin/sh
# Installs the fork-ci pre-push hook into this clone's shared hooks directory (every worktree uses it).
set -eu
src="$(cd "$(dirname "$0")" && pwd)/pre-push"
hooks="$(git rev-parse --git-common-dir)/hooks"
mkdir -p "$hooks"
if [ -e "$hooks/pre-push" ] && ! cmp -s "$src" "$hooks/pre-push"; then
  echo "error: $hooks/pre-push exists and differs; merge it by hand" >&2
  exit 1
fi
cp "$src" "$hooks/pre-push"
chmod +x "$hooks/pre-push"
echo "installed $hooks/pre-push"
