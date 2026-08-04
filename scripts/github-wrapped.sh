#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
STANDALONE_DIR="$ROOT_DIR/crates/frontend/standalone"
YEAR="$(date +%Y)"
LOGIN="${GITHUB_WRAPPED_LOGIN:-}"
OUT=""
FROM_JSON=""
RAW_JSON_OUT=""
LIST_YEARS="false"
INCLUDE_PRIVATE="false"

fail() {
  echo "[github-wrapped][ERROR] $*" >&2
  exit 1
}

usage() {
  cat <<'EOF'
Usage:
  scripts/github-wrapped.sh --list-years [--login <github-login>]
  scripts/github-wrapped.sh --year <year> [--login <github-login>]

Options:
  --login <login>          GitHub login to render. Defaults to GITHUB_WRAPPED_LOGIN
                           or the owner inferred from git remote origin.
  --year <year>            Wrapped year. Defaults to the current year.
  --out <path>             Output HTML path. Defaults to
                           crates/frontend/standalone/github-wrapped-<year>.html.
  --standalone-dir <dir>   Directory for wrapped HTML and manifest.
  --from-json <path>       Render from a saved dataset JSON instead of calling GitHub.
  --raw-json-out <path>    Save the collected GitHub dataset JSON.
  --include-private        Use /user/repos and include private repos visible to token.
  --list-years             List locally generated wrapped years for the selected login.
  -h, --help               Show this help.

Environment:
  GH_TOKEN or GITHUB_TOKEN is required when --from-json is not used.
  GITHUB_WRAPPED_LOGIN can set the default login.

Examples:
  GH_TOKEN=... GITHUB_WRAPPED_LOGIN=zero-yx scripts/github-wrapped.sh --year 2026
  GH_TOKEN=... scripts/github-wrapped.sh --login zero-yx --year 2025
  scripts/github-wrapped.sh --login zero-yx --list-years

After generating, rebuild the frontend:
  bash scripts/build_frontend_selfhosted.sh
EOF
}

infer_login_from_origin() {
  local remote
  remote="$(git -C "$ROOT_DIR" remote get-url origin 2>/dev/null || true)"
  remote="${remote%.git}"

  case "$remote" in
    https://github.com/*/*)
      remote="${remote#https://github.com/}"
      echo "${remote%%/*}"
      ;;
    git@github.com:*/*)
      remote="${remote#git@github.com:}"
      echo "${remote%%/*}"
      ;;
    ssh://git@github.com/*/*)
      remote="${remote#ssh://git@github.com/}"
      echo "${remote%%/*}"
      ;;
  esac
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --login)
      [[ $# -ge 2 ]] || fail "--login requires a value"
      LOGIN="$2"
      shift 2
      ;;
    --year)
      [[ $# -ge 2 ]] || fail "--year requires a value"
      YEAR="$2"
      shift 2
      ;;
    --out)
      [[ $# -ge 2 ]] || fail "--out requires a value"
      OUT="$2"
      shift 2
      ;;
    --standalone-dir)
      [[ $# -ge 2 ]] || fail "--standalone-dir requires a value"
      STANDALONE_DIR="$2"
      shift 2
      ;;
    --from-json)
      [[ $# -ge 2 ]] || fail "--from-json requires a value"
      FROM_JSON="$2"
      shift 2
      ;;
    --raw-json-out)
      [[ $# -ge 2 ]] || fail "--raw-json-out requires a value"
      RAW_JSON_OUT="$2"
      shift 2
      ;;
    --include-private)
      INCLUDE_PRIVATE="true"
      shift
      ;;
    --list-years)
      LIST_YEARS="true"
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      fail "Unknown option: $1 (use --help)"
      ;;
  esac
done

if [[ -z "$LOGIN" ]]; then
  LOGIN="$(infer_login_from_origin || true)"
fi
[[ -n "$LOGIN" ]] || fail "Set --login or GITHUB_WRAPPED_LOGIN"

if ! [[ "$YEAR" =~ ^[0-9]{4}$ ]]; then
  fail "--year must be a four-digit year, got: $YEAR"
fi

if [[ "$LIST_YEARS" == "true" ]]; then
  shopt -s nullglob
  years=()
  for file in "$STANDALONE_DIR"/github-wrapped-*.html; do
    if grep -Fq "data-github-wrapped-login=\"$LOGIN\"" "$file"; then
      base="${file##*/}"
      year="${base#github-wrapped-}"
      year="${year%.html}"
      years+=("$year")
    fi
  done
  if [[ ${#years[@]} -eq 0 ]]; then
    echo "No GitHub Wrapped pages found for $LOGIN in $STANDALONE_DIR"
  else
    printf "%s\n" "${years[@]}" | sort -r
  fi
  exit 0
fi

if [[ -z "$OUT" ]]; then
  OUT="$STANDALONE_DIR/github-wrapped-$YEAR.html"
fi

if [[ -z "$FROM_JSON" ]]; then
  TOKEN="${GH_TOKEN:-${GITHUB_TOKEN:-}}"
  [[ -n "$TOKEN" ]] || fail "GH_TOKEN or GITHUB_TOKEN is required unless --from-json is used"
else
  TOKEN=""
  [[ -f "$FROM_JSON" ]] || fail "--from-json file not found: $FROM_JSON"
fi

mkdir -p "$STANDALONE_DIR" "$(dirname "$OUT")"
if [[ -n "$RAW_JSON_OUT" ]]; then
  mkdir -p "$(dirname "$RAW_JSON_OUT")"
fi

python3 - "$LOGIN" "$YEAR" "$OUT" "$STANDALONE_DIR" "$FROM_JSON" "$RAW_JSON_OUT" "$INCLUDE_PRIVATE" <<'PY'
import datetime as dt
import html
import json
import os
import re
import sys
import urllib.error
import urllib.parse
import urllib.request

login, year, out_path, standalone_dir, from_json, raw_json_out, include_private = sys.argv[1:8]
include_private = include_private == "true"
token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN") or ""
api_base = "https://api.github.com"


def fail(message):
    print(f"[github-wrapped][ERROR] {message}", file=sys.stderr)
    sys.exit(1)


def request_json(path, *, method="GET", payload=None, accept="application/vnd.github+json"):
    if path.startswith("https://"):
        url = path
    else:
        url = api_base + path
    body = None
    headers = {
        "Accept": accept,
        "User-Agent": "staticflow-github-wrapped",
        "X-GitHub-Api-Version": "2022-11-28",
    }
    if token:
        headers["Authorization"] = f"Bearer {token}"
    if payload is not None:
        body = json.dumps(payload).encode("utf-8")
        headers["Content-Type"] = "application/json"

    req = urllib.request.Request(url, data=body, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            data = resp.read().decode("utf-8")
            return json.loads(data), resp.headers
    except urllib.error.HTTPError as exc:
        detail = exc.read().decode("utf-8", errors="replace")
        fail(f"GitHub API request failed: {exc.code} {exc.reason} {url}\n{detail}")
    except urllib.error.URLError as exc:
        fail(f"GitHub API request failed: {url}: {exc.reason}")


def paginate(path):
    items = []
    next_url = path
    while next_url:
        data, headers = request_json(next_url)
        if not isinstance(data, list):
            fail(f"Expected list response from GitHub API: {next_url}")
        items.extend(data)
        next_url = None
        link_header = headers.get("Link", "")
        for part in link_header.split(","):
            if 'rel="next"' not in part:
                continue
            match = re.search(r"<([^>]+)>", part)
            if match:
                next_url = match.group(1)
                break
    return items


def graphql_contributions():
    start = f"{year}-01-01T00:00:00Z"
    end = f"{year}-12-31T23:59:59Z"
    query = """
    query($login: String!, $from: DateTime!, $to: DateTime!) {
      user(login: $login) {
        contributionsCollection(from: $from, to: $to) {
          contributionCalendar {
            totalContributions
            weeks {
              contributionDays {
                date
                contributionCount
              }
            }
          }
          totalCommitContributions
          totalIssueContributions
          totalPullRequestContributions
          totalPullRequestReviewContributions
          totalRepositoryContributions
        }
      }
    }
    """
    payload = {
        "query": query,
        "variables": {"login": login, "from": start, "to": end},
    }
    data, _ = request_json("/graphql", method="POST", payload=payload)
    if data.get("errors"):
        fail("GitHub GraphQL request failed: " + json.dumps(data["errors"], ensure_ascii=False))
    user = (data.get("data") or {}).get("user")
    if not user:
        fail(f"GitHub user not found: {login}")
    collection = user.get("contributionsCollection") or {}
    calendar = collection.get("contributionCalendar") or {}
    days = []
    for week in calendar.get("weeks") or []:
        days.extend(week.get("contributionDays") or [])
    return {
        "totalContributions": calendar.get("totalContributions", 0),
        "totalCommitContributions": collection.get("totalCommitContributions", 0),
        "totalIssueContributions": collection.get("totalIssueContributions", 0),
        "totalPullRequestContributions": collection.get("totalPullRequestContributions", 0),
        "totalPullRequestReviewContributions": collection.get("totalPullRequestReviewContributions", 0),
        "totalRepositoryContributions": collection.get("totalRepositoryContributions", 0),
        "days": days,
    }


def fetch_dataset():
    profile, _ = request_json(f"/users/{urllib.parse.quote(login)}")
    if include_private:
        repos = paginate("/user/repos?per_page=100&visibility=all&affiliation=owner&sort=pushed")
        repos = [
            repo for repo in repos
            if (repo.get("owner") or {}).get("login", "").lower() == login.lower()
        ]
    else:
        repos = paginate(
            f"/users/{urllib.parse.quote(login)}/repos?per_page=100&type=owner&sort=pushed"
        )

    languages = {}
    for repo in repos[:80]:
        full_name = repo.get("full_name")
        if not full_name or repo.get("fork"):
            continue
        encoded = urllib.parse.quote(full_name, safe="/")
        try:
            repo_languages, _ = request_json(f"/repos/{encoded}/languages")
        except SystemExit:
            raise
        except Exception:
            repo_languages = {}
        if isinstance(repo_languages, dict):
            for name, bytes_count in repo_languages.items():
                languages[name] = languages.get(name, 0) + int(bytes_count or 0)

    return {
        "profile": profile,
        "repos": repos,
        "languages": languages,
        "contributions": graphql_contributions(),
    }


def load_dataset():
    if from_json:
        with open(from_json, "r", encoding="utf-8") as fh:
            return json.load(fh)
    dataset = fetch_dataset()
    if raw_json_out:
        with open(raw_json_out, "w", encoding="utf-8") as fh:
            json.dump(dataset, fh, ensure_ascii=False, indent=2, sort_keys=True)
            fh.write("\n")
    return dataset


def parse_year(value):
    if not value:
        return None
    return str(value)[:4]


def fmt_date(value):
    if not value:
        return "-"
    return str(value)[:10]


def esc(value):
    return html.escape(str(value or ""), quote=True)


def compact_number(value):
    try:
        value = int(value or 0)
    except (TypeError, ValueError):
        value = 0
    if value >= 1_000_000:
        return f"{value / 1_000_000:.1f}M"
    if value >= 1_000:
        return f"{value / 1_000:.1f}k"
    return str(value)


def repo_year_match(repo, field):
    return parse_year(repo.get(field)) == year


def normalize_dataset(dataset):
    profile = dataset.get("profile") or {}
    repos = dataset.get("repos") or []
    languages = dataset.get("languages") or {}
    contributions = dataset.get("contributions") or {}

    repos = [repo for repo in repos if isinstance(repo, dict)]
    languages = {str(k): int(v or 0) for k, v in languages.items()}
    return profile, repos, languages, contributions


def render_html(dataset):
    profile, repos, languages, contributions = normalize_dataset(dataset)
    display_name = profile.get("name") or profile.get("login") or login
    html_url = profile.get("html_url") or f"https://github.com/{login}"
    avatar_url = profile.get("avatar_url") or ""

    active_repos = [
        repo for repo in repos
        if repo_year_match(repo, "pushed_at") or repo_year_match(repo, "updated_at")
    ]
    created_repos = [repo for repo in repos if repo_year_match(repo, "created_at")]
    non_fork_repos = [repo for repo in repos if not repo.get("fork")]
    total_stars = sum(int(repo.get("stargazers_count") or 0) for repo in repos)
    total_forks = sum(int(repo.get("forks_count") or 0) for repo in repos)
    top_repos = sorted(
        repos,
        key=lambda repo: (
            int(repo.get("stargazers_count") or 0),
            int(repo.get("forks_count") or 0),
            repo.get("pushed_at") or "",
        ),
        reverse=True,
    )[:8]
    recent_repos = sorted(repos, key=lambda repo: repo.get("pushed_at") or "", reverse=True)[:8]

    language_total = sum(languages.values())
    language_rows = sorted(languages.items(), key=lambda item: item[1], reverse=True)[:8]
    if not language_rows:
        fallback = {}
        for repo in non_fork_repos:
            lang = repo.get("language")
            if lang:
                fallback[lang] = fallback.get(lang, 0) + 1
        language_rows = sorted(fallback.items(), key=lambda item: item[1], reverse=True)[:8]
        language_total = sum(count for _, count in language_rows)

    contribution_total = int(contributions.get("totalContributions") or 0)
    generated_at = dt.datetime.now(dt.timezone.utc).replace(microsecond=0).isoformat()

    def stat_card(label, value, detail=""):
        return (
            '<div class="card stat">'
            f'<div class="label">{esc(label)}</div>'
            f'<div class="value">{esc(value)}</div>'
            f'<div class="detail">{esc(detail)}</div>'
            '</div>'
        )

    def repo_card(repo):
        full_name = repo.get("full_name") or repo.get("name") or "-"
        description = repo.get("description") or "No description"
        url = repo.get("html_url") or f"https://github.com/{full_name}"
        lang = repo.get("language") or "Mixed"
        return f"""
        <a class="repo" href="{esc(url)}" target="_blank" rel="noopener noreferrer">
          <div class="repo-title">{esc(full_name)}</div>
          <div class="repo-desc">{esc(description)}</div>
          <div class="repo-meta">
            <span>{esc(lang)}</span>
            <span>stars {compact_number(repo.get("stargazers_count"))}</span>
            <span>pushed {esc(fmt_date(repo.get("pushed_at")))}</span>
          </div>
        </a>
        """

    language_html = ""
    for name, amount in language_rows:
        pct = 0 if language_total == 0 else round((amount / language_total) * 100)
        language_html += f"""
        <div class="language-row">
          <div class="language-head">
            <span>{esc(name)}</span><span>{pct}%</span>
          </div>
          <div class="bar"><span style="width: {pct}%"></span></div>
        </div>
        """

    return f"""<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>{esc(login)} · GitHub Wrapped {esc(year)}</title>
  <meta name="description" content="GitHub Wrapped {esc(year)} for {esc(login)}" />
  <style>
    :root {{
      color-scheme: dark;
      --bg: #0b0f14;
      --panel: #111827;
      --panel-2: #172033;
      --text: #eef2ff;
      --muted: #9ca3af;
      --line: rgba(148, 163, 184, .22);
      --accent: #38bdf8;
      --accent-2: #a3e635;
    }}
    * {{ box-sizing: border-box; }}
    body {{
      margin: 0;
      min-height: 100vh;
      background:
        radial-gradient(circle at 20% 0%, rgba(56, 189, 248, .16), transparent 34rem),
        radial-gradient(circle at 90% 10%, rgba(163, 230, 53, .10), transparent 28rem),
        var(--bg);
      color: var(--text);
      font-family: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
    }}
    main {{ width: min(1120px, calc(100% - 32px)); margin: 0 auto; padding: 48px 0 72px; }}
    .hero {{ display: grid; gap: 22px; padding: 32px 0 26px; border-bottom: 1px solid var(--line); }}
    .profile {{ display: flex; align-items: center; gap: 18px; }}
    .avatar {{ width: 74px; height: 74px; border-radius: 18px; border: 1px solid var(--line); background: var(--panel); object-fit: cover; }}
    .eyebrow {{ color: var(--accent-2); font: 700 12px/1 ui-monospace, SFMono-Regular, Menlo, monospace; letter-spacing: .12em; text-transform: uppercase; }}
    h1 {{ margin: 8px 0 0; font-size: clamp(42px, 8vw, 92px); line-height: .9; letter-spacing: 0; }}
    .sub {{ max-width: 760px; margin: 0; color: var(--muted); font-size: 18px; line-height: 1.7; }}
    .grid {{ display: grid; gap: 16px; grid-template-columns: repeat(4, minmax(0, 1fr)); margin-top: 26px; }}
    .card {{ border: 1px solid var(--line); border-radius: 8px; background: linear-gradient(180deg, rgba(255,255,255,.04), rgba(255,255,255,.02)); padding: 18px; }}
    .label {{ color: var(--muted); font-size: 12px; text-transform: uppercase; letter-spacing: .1em; }}
    .value {{ margin-top: 8px; font-size: 34px; font-weight: 800; }}
    .detail {{ margin-top: 8px; color: var(--muted); font-size: 13px; min-height: 18px; }}
    section {{ margin-top: 34px; }}
    h2 {{ margin: 0 0 16px; font-size: 24px; }}
    .split {{ display: grid; gap: 18px; grid-template-columns: 1fr 1fr; }}
    .language-row + .language-row {{ margin-top: 14px; }}
    .language-head {{ display: flex; justify-content: space-between; margin-bottom: 7px; color: var(--text); font-size: 14px; }}
    .bar {{ height: 9px; border-radius: 999px; background: rgba(148, 163, 184, .18); overflow: hidden; }}
    .bar span {{ display: block; height: 100%; border-radius: inherit; background: linear-gradient(90deg, var(--accent), var(--accent-2)); }}
    .repos {{ display: grid; gap: 12px; }}
    .repo {{ display: block; color: inherit; text-decoration: none; border: 1px solid var(--line); border-radius: 8px; padding: 16px; background: rgba(17,24,39,.68); }}
    .repo:hover {{ border-color: rgba(56, 189, 248, .65); }}
    .repo-title {{ font-weight: 800; }}
    .repo-desc {{ margin-top: 7px; color: var(--muted); line-height: 1.5; }}
    .repo-meta {{ display: flex; flex-wrap: wrap; gap: 10px; margin-top: 11px; color: var(--muted); font: 12px/1.4 ui-monospace, SFMono-Regular, Menlo, monospace; }}
    .footer {{ margin-top: 42px; color: var(--muted); font-size: 13px; }}
    .footer a {{ color: var(--accent); }}
    @media (max-width: 800px) {{
      .grid, .split {{ grid-template-columns: 1fr; }}
      .profile {{ align-items: flex-start; }}
    }}
  </style>
</head>
<body data-github-wrapped-login="{esc(login)}" data-generated-by="staticflow-github-wrapped">
  <main>
    <header class="hero">
      <div class="profile">
        <img class="avatar" src="{esc(avatar_url)}" alt="{esc(login)} avatar" />
        <div>
          <div class="eyebrow">GitHub Wrapped · {esc(year)}</div>
          <h1>{esc(display_name)}</h1>
        </div>
      </div>
      <p class="sub">
        A static snapshot generated from GitHub API data for @{esc(login)}.
        Public data is used by default; private repositories are included only when
        --include-private is passed and the token can read them.
      </p>
      <div class="grid">
        {stat_card("Contributions", compact_number(contribution_total), "calendar total")}
        {stat_card("Active repos", compact_number(len(active_repos)), f"pushed or updated in {year}")}
        {stat_card("Created repos", compact_number(len(created_repos)), f"new in {year}")}
        {stat_card("Stars", compact_number(total_stars), f"forks {compact_number(total_forks)}")}
      </div>
    </header>

    <section class="split">
      <div class="card">
        <h2>Contribution Mix</h2>
        <div class="grid" style="grid-template-columns: repeat(2, minmax(0, 1fr)); margin-top: 0;">
          {stat_card("Commits", compact_number(contributions.get("totalCommitContributions")), "")}
          {stat_card("Pull requests", compact_number(contributions.get("totalPullRequestContributions")), "")}
          {stat_card("Reviews", compact_number(contributions.get("totalPullRequestReviewContributions")), "")}
          {stat_card("Issues", compact_number(contributions.get("totalIssueContributions")), "")}
        </div>
      </div>
      <div class="card">
        <h2>Language Signal</h2>
        {language_html or '<p class="sub">No language data found.</p>'}
      </div>
    </section>

    <section>
      <h2>Top Repositories</h2>
      <div class="repos">
        {''.join(repo_card(repo) for repo in top_repos) or '<div class="card">No repositories found.</div>'}
      </div>
    </section>

    <section>
      <h2>Recently Pushed</h2>
      <div class="repos">
        {''.join(repo_card(repo) for repo in recent_repos) or '<div class="card">No repositories found.</div>'}
      </div>
    </section>

    <p class="footer">
      Generated at {esc(generated_at)}.
      <a href="{esc(html_url)}" target="_blank" rel="noopener noreferrer">Open @{esc(login)} on GitHub</a>.
    </p>
  </main>
</body>
</html>
"""


def write_manifest():
    manifest_path = os.path.join(standalone_dir, "github-wrapped-manifest.json")
    existing = {}
    if os.path.exists(manifest_path):
        try:
            with open(manifest_path, "r", encoding="utf-8") as fh:
                existing = json.load(fh)
        except (OSError, json.JSONDecodeError):
            existing = {}

    years = [
        entry for entry in existing.get("years", [])
        if str(entry.get("login", login)).lower() == login.lower()
        and int(entry.get("year", -1)) != int(year)
    ]
    years.append({
        "year": int(year),
        "login": login,
        "url": f"/standalone/github-wrapped-{year}.html",
    })
    years.sort(key=lambda entry: int(entry.get("year", 0)), reverse=True)
    for idx, entry in enumerate(years):
        entry["is_latest"] = idx == 0

    manifest = {
        "login": login,
        "generated_at": dt.datetime.now(dt.timezone.utc).replace(microsecond=0).isoformat(),
        "years": years,
    }
    with open(manifest_path, "w", encoding="utf-8") as fh:
        json.dump(manifest, fh, ensure_ascii=False, indent=2, sort_keys=True)
        fh.write("\n")
    return manifest_path


dataset = load_dataset()
html_text = render_html(dataset)
with open(out_path, "w", encoding="utf-8") as fh:
    fh.write(html_text)
manifest_path = write_manifest()

print(f"[github-wrapped] wrote {out_path}")
print(f"[github-wrapped] wrote {manifest_path}")
PY
