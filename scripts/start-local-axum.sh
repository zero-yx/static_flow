#!/usr/bin/env bash
# Run under WSL. The admin surface is never forwarded by the reading site.
set -euo pipefail
repo="$(cd "$(dirname "$0")/.." && pwd)"
base="${STATICFLOW_LOCAL_RUNTIME:-$HOME/projects/control-room-staticflow}"
bin="${STATICFLOW_LOCAL_BIN:-$base/backend-target/debug}"
state="$base/private-runtime"
for f in "$bin/static-flow-backend" "$bin/sf-cli" "$repo/.local/site/index.html"; do test -f "$f" || { echo "Missing $f" >&2; exit 1; }; done
if curl --silent --fail --max-time 2 http://127.0.0.1:39082/api/healthz >/dev/null; then
  test -f "$state/server.pid" && test "$(readlink "/proc/$(cat "$state/server.pid")/exe")" = "$bin/static-flow-backend" || { echo 'Unrelated listener at 39082' >&2; exit 1; }
  echo 'http://127.0.0.1:39082/api/healthz'; exit 0
fi
mkdir -p "$state"/{lancedb,comments,music,home,log}
cd "$state"
if ! test -d lancedb/articles.lance; then
  env -i PATH=/usr/bin:/bin HOME="$state/home" "$bin/sf-cli" init --db-path "$state/lancedb"
fi
umask 077
if ! test -s admin.key; then python3 -c 'import secrets;print(secrets.token_urlsafe(32))' > admin.key; fi
env -i PATH=/usr/bin:/bin HOME="$state/home" BIND_ADDR=127.0.0.1 PORT=39082 SITE_BASE_URL=http://127.0.0.1:39082 \
    LANCEDB_URI="$state/lancedb" COMMENTS_LANCEDB_URI="$state/comments" MUSIC_LANCEDB_URI="$state/music" \
    FRONTEND_DIST_DIR="$repo/.local/site" STATICFLOW_LOG_DIR="$state/log" ADMIN_LOCAL_ONLY=true ADMIN_TOKEN="$(cat admin.key)" \
    ENABLE_GEOIP_AUTO_DOWNLOAD=false ENABLE_GEOIP_FALLBACK_API=false TABLE_COMPACT_ENABLED=false \
    COMMENT_AI_RUNNER_PROGRAM=/bin/false MUSIC_WISH_RUNNER_PROGRAM=/bin/false ARTICLE_REQUEST_RUNNER_PROGRAM=/bin/false \
    ORT_DYLIB_PATH="${ORT_DYLIB_PATH:-libonnxruntime.so}" "$bin/static-flow-backend" < /dev/null > "$state/log/boot.log" 2>&1 &
pid=$!; echo "$pid" > server.pid
for _ in $(seq 1 45); do
  if curl -fsS --max-time 2 http://127.0.0.1:39082/api/healthz 2>/dev/null; then echo; wait "$pid"; exit $?; fi
  kill -0 "$pid" 2>/dev/null || { echo "See $state/log/boot.log" >&2; exit 1; }; sleep 1
done
echo "See $state/log/boot.log" >&2; exit 1
