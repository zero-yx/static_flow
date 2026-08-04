#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="$ROOT_DIR/scripts/github-wrapped.sh"
WORKDIR="$ROOT_DIR/tmp/github-wrapped-test"

fail() {
  echo "[github-wrapped-test][FAIL] $*" >&2
  exit 1
}

assert_contains() {
  local haystack="$1"
  local needle="$2"
  local message="$3"
  if [[ "$haystack" != *"$needle"* ]]; then
    fail "$message (missing: $needle)"
  fi
}

assert_not_contains() {
  local haystack="$1"
  local needle="$2"
  local message="$3"
  if [[ "$haystack" == *"$needle"* ]]; then
    fail "$message (unexpected: $needle)"
  fi
}

rm -rf "$WORKDIR"
mkdir -p "$WORKDIR/standalone"

help_output="$("$SCRIPT" --help)"
assert_contains "$help_output" "--login" "help should document --login"
assert_contains "$help_output" "GH_TOKEN" "help should document token env"

cat > "$WORKDIR/standalone/github-wrapped-2024.html" <<'HTML'
<!doctype html><html><body data-github-wrapped-login="acking-you"></body></html>
HTML
cat > "$WORKDIR/standalone/github-wrapped-2025.html" <<'HTML'
<!doctype html><html><body data-github-wrapped-login="zero-yx"></body></html>
HTML

years_output="$(
  GITHUB_WRAPPED_LOGIN=zero-yx \
    "$SCRIPT" --standalone-dir "$WORKDIR/standalone" --list-years
)"
assert_contains "$years_output" "2025" "list-years should include matching pages"
assert_not_contains "$years_output" "2024" "list-years should filter out other users"

if env -u GH_TOKEN -u GITHUB_TOKEN GITHUB_WRAPPED_LOGIN=zero-yx \
  "$SCRIPT" --standalone-dir "$WORKDIR/standalone" --year 2026 --out "$WORKDIR/no-token.html" \
  >"$WORKDIR/no-token.out" 2>"$WORKDIR/no-token.err"; then
  fail "generation without token should fail"
fi
assert_contains "$(cat "$WORKDIR/no-token.err")" "GH_TOKEN or GITHUB_TOKEN" \
  "missing token error should be actionable"

cat > "$WORKDIR/fixture.json" <<'JSON'
{
  "profile": {
    "login": "zero-yx",
    "name": "Lancer",
    "avatar_url": "https://example.com/avatar.png",
    "html_url": "https://github.com/zero-yx",
    "public_repos": 2
  },
  "repos": [
    {
      "name": "static_flow",
      "full_name": "zero-yx/static_flow",
      "html_url": "https://github.com/zero-yx/static_flow",
      "description": "Personal StaticFlow fork",
      "stargazers_count": 3,
      "forks_count": 1,
      "language": "Rust",
      "fork": false,
      "created_at": "2026-01-02T00:00:00Z",
      "pushed_at": "2026-03-04T00:00:00Z"
    }
  ],
  "languages": {
    "Rust": 1200,
    "TypeScript": 400
  },
  "contributions": {
    "totalContributions": 42,
    "totalCommitContributions": 20,
    "totalIssueContributions": 4,
    "totalPullRequestContributions": 10,
    "totalPullRequestReviewContributions": 8,
    "totalRepositoryContributions": 1
  }
}
JSON

GITHUB_WRAPPED_LOGIN=zero-yx \
  "$SCRIPT" --standalone-dir "$WORKDIR/standalone" --year 2026 \
  --from-json "$WORKDIR/fixture.json" --out "$WORKDIR/github-wrapped-2026.html"

generated="$(cat "$WORKDIR/github-wrapped-2026.html")"
assert_contains "$generated" 'data-github-wrapped-login="zero-yx"' \
  "generated page should carry the owner marker"
assert_contains "$generated" "Lancer" "generated page should include profile name"
assert_not_contains "$generated" "acking-you" "generated page should not leak original owner"

echo "[github-wrapped-test] ok"
