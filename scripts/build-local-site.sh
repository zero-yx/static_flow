#!/usr/bin/env bash
# Run under WSL from this checkout. Keep the full backend workspace intact.
set -euo pipefail
repo="$(cd "$(dirname "$0")/.." && pwd)"
build="${XDG_CACHE_HOME:-$HOME/.cache}/zero-yx-staticflow"
mkdir -p "$build" "$repo/.local"
stage="$(mktemp -d "$build/src.XXXXXXXX")"
out="$(mktemp -d "$repo/.local/site-build.XXXXXXXX")"
revision="$(git -C "$repo" -c safe.directory="$repo" rev-parse HEAD)"
git -C "$repo" -c safe.directory="$repo" archive "$revision" | tar -xf - -C "$stage"
cd "$stage"
bash scripts/prepare_frontend_only_workspace.sh --force
cd crates/frontend
npm ci --no-audit
export CARGO_TARGET_DIR="$build/target" TRUNK_SKIP_VERSION_CHECK=true STATICFLOW_API_BASE=/api
trunk build --release --features personal-notes
cp -a dist/. "$out/"
cp -a standalone "$out/"
test -s "$out/index.html"
test -s "$out/standalone/github-wrapped-2026.html"
# Publish a complete directory. Preserve the previous copy for recovery.
if test -d "$repo/.local/site"; then
  previous="$(mktemp -d "$repo/.local/site-previous.XXXXXXXX")"
  mv -T "$repo/.local/site" "$previous"
  if ! mv -T "$out" "$repo/.local/site"; then mv -T "$previous" "$repo/.local/site"; exit 1; fi
else
  mv -T "$out" "$repo/.local/site"
fi
echo "Published $repo/.local/site from $revision"
