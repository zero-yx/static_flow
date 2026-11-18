"""Serve the built Yew app with a read-only view of the site's Markdown notes."""
from collections import Counter
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
from urllib.parse import urlsplit, parse_qs, unquote

ROOT = Path(__file__).resolve().parents[1]
DIST = ROOT / ".local" / "site"
NOTES = ROOT / "content" / "site-notes"


def articles():
    rows = []
    for file in NOTES.glob("*.json"):
        meta = json.loads(file.read_text("utf-8"))
        body = file.with_suffix(".md").read_text("utf-8")
        rows.append(dict(meta, id=file.stem, content=body, author="zero-yx",
                         read_time=max(1, len(body) // 350), featured_image=None))
    return sorted(rows, key=lambda row: (row["date"], row["id"]), reverse=True)


class Site(SimpleHTTPRequestHandler):
    extensions_map = {**SimpleHTTPRequestHandler.extensions_map, ".wasm": "application/wasm", ".js": "text/javascript"}

    def list_directory(self, path):
        return self.send_error(404)

    def object(self, obj, code=200, raw=False):
        body = (obj if raw else json.dumps(obj, ensure_ascii=False)).encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", ("text/markdown" if raw else "application/json") + "; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)

    def do_GET(self):
        if self.headers.get("Host") not in {"localhost:39080", "127.0.0.1:39080"}:
            return self.send_error(403)
        url = urlsplit(self.path)
        name = unquote(url.path)
        if name == "/healthz":
            return self.object(dict(service="static-flow/zero-yx", mode="personal-notes", articles=len(articles())))
        if name.startswith("/api/"):
            params = {k: xs[0] for k, xs in parse_qs(url.query).items()}
            rows = articles()
            tags = Counter(t for r in rows for t in r["tags"])
            cats = Counter(r["category"] for r in rows)
            if name == "/api/stats":
                return self.object(dict(total_articles=len(rows), total_tags=len(tags), total_categories=len(cats)))
            if name == "/api/tags":
                return self.object(dict(tags=[dict(name=k, count=v) for k,v in tags.items()]))
            if name == "/api/categories":
                return self.object(dict(categories=[dict(name=k,count=v,description="") for k,v in cats.items()]))
            if name == "/api/search":
                q = params.get("q","")
                matches = [{**r, "highlight":r["summary"]} for r in rows if q.casefold() in (r["title"] + r["content"]).casefold()]
                return self.object(dict(query=q, total=len(matches), results=matches))
            try:
                offset = max(0, int(params.get("offset", 0)))
                limit = min(1000, max(1, int(params.get("limit", 100))))
            except ValueError:
                return self.object({"error": "invalid pagination"}, 400)
            if name == "/api/articles":
                matches = [r for r in rows if params.get("category",r["category"]) == r["category"] and
                           ("tag" not in params or params["tag"] in r["tags"])]
                return self.object(dict(articles=matches[offset:offset+limit], total=len(matches), limit=limit, offset=offset, has_more=offset+limit<len(matches)))
            if name == "/api/music/recommendations/random":
                return self.object([])
            if name in {"/api/music", "/api/images", "/api/images/random"}:
                return self.object({"songs" if name == "/api/music" else "images": [], "limit": limit, "offset":offset, "has_more":False, "total":0})
            for row in rows:
                base = "/api/articles/" + row["id"]
                if name == base:
                    return self.object(row)
                if name == base + "/raw/zh":
                    return self.object(row["content"],raw=True)
                if name == base + "/related":
                    return self.object(dict(articles=[],total=0))
            return self.object(dict(error="本机只读文章版未启用此 API"), 404)
        file = Path(self.translate_path(self.path)).resolve()
        if file != DIST and DIST not in file.parents or any(p.startswith(".") for p in name.split("/")):
            return self.send_error(404)
        if not file.exists() and not file.suffix and not name.startswith(("/static/", "/standalone/")):
            self.path = "/index.html"
        return super().do_GET() if self.command != "HEAD" else super().do_HEAD()

    do_HEAD = do_GET


if __name__ == "__main__":
    if not (DIST / "index.html").is_file():
        raise SystemExit("先将本分支编译到 .local/site")
    ThreadingHTTPServer(("127.0.0.1",39080),partial(Site, directory=str(DIST))).serve_forever()
