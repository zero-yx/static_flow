#!/usr/bin/env bash
set -euo pipefail

# Prepare a checked-out CI workspace for frontend-only builds.
#
# The full StaticFlow workspace references vendored/path dependencies under
# deps/ and patches/. Those repositories are not required by the WASM frontend
# deploy job, and a fork may not have access to clone the original author's
# private submodules. This script narrows the root workspace in-place so Trunk
# can resolve only the crates needed by crates/frontend.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FORCE="false"

if [[ "${1:-}" == "--force" ]]; then
  FORCE="true"
fi

if [[ "${CI:-}" != "true" && "$FORCE" != "true" ]]; then
  echo "[frontend-only-workspace][ERROR] Refusing to modify Cargo.toml outside CI." >&2
  echo "Use --force only in a disposable working tree." >&2
  exit 1
fi

python3 - "$ROOT_DIR/Cargo.toml" <<'PY'
from pathlib import Path
import re
import sys

path = Path(sys.argv[1])
text = path.read_text()

frontend_members = """members = [
    "crates/frontend",
    "crates/shared",
    "crates/media-types",
    "crates/llm-access-core",
]"""

start = text.find("members = [")
if start == -1:
    raise SystemExit("members array not found in Cargo.toml")

end = text.find("]\n", start)
if end == -1:
    raise SystemExit("members array closing bracket not found in Cargo.toml")
end += 2

text = text[:start] + frontend_members + text[end:]

text = text.replace(
    'lance = { path = "deps/lance/rust/lance", default-features = false }\n',
    "",
)
text = text.replace(
    'lancedb = { path = "deps/lancedb/rust/lancedb" }\n',
    "",
)

text = re.sub(
    r"\n# Patch: fix object_store LocalUpload::complete\(\) fstat-after-rename\n"
    r"# failure on WSL2 9p/drvfs \(NTFS mounts\)\n"
    r"\[patch\.crates-io\]\n"
    r'object_store = \{ path = "patches/object_store" \}\n',
    "\n",
    text,
)

path.write_text(text)
print("[frontend-only-workspace] Root Cargo.toml narrowed for frontend deploy")
PY
