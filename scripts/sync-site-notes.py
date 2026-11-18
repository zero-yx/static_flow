"""Run in WSL; copy only explicitly published Markdown notes into local LanceDB."""
import json, os, subprocess, tempfile
from pathlib import Path

repo = Path(__file__).resolve().parents[1]
base = Path(os.environ.get("STATICFLOW_LOCAL_RUNTIME", str(Path.home()/"projects/control-room-staticflow")))
bin_dir = Path(os.environ.get("STATICFLOW_LOCAL_BIN", str(base/"backend-target/debug")))
state = base / "private-runtime"
if not (state/"lancedb/articles.lance").is_dir():
    raise SystemExit("先启动本机 Axum 并初始化专属数据库")
for meta in sorted((repo/"content/site-notes").glob("*.json")):
    data = json.loads(meta.read_text("utf-8"))
    fields = {**data, "author":"zero-yx", "category_description":data["category"]}
    body = meta.with_suffix(".md").read_text("utf-8")
    with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", suffix=".md") as file:
        file.write("---\n")
        for k, v in fields.items():
            file.write(k + ": " + json.dumps(v,ensure_ascii=False) + "\n")
        file.write("---\n\n"+body); file.flush()
        subprocess.run([str(bin_dir/"sf-cli"), "write-article", "--db-path", str(state/"lancedb"),
                        "--file", file.name, "--id", meta.stem, "--no-auto-optimize"],
                       check=True, cwd=state, env={"HOME":str(state/"home"), "PATH":"/usr/bin:/bin",
                                                    "STATICFLOW_SKIP_TEXT_EMBEDDING":"1"})
