#!/usr/bin/env bash
# Installs scripts/pre-commit into this clone. Safe to re-run.
set -euo pipefail
root=$(git rev-parse --show-toplevel)
hooks=$(git -C "$root" rev-parse --path-format=absolute --git-common-dir)/hooks
mkdir -p "$hooks"
target="$hooks/pre-commit"
if [ -e "$target" ] && [ ! -L "$target" ]; then
    echo "Leaving $target alone: it is a file, not our symlink. Move it aside and re-run." >&2; exit 1
fi
ln -sf "$root/scripts/pre-commit" "$target"
echo "pre-commit hook installed: $target"
echo "Optional: put personal patterns (one regex per line) in ~/.config/kestrel/denylist"
