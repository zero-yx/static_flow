# 个人 fork 部署说明

这份说明面向 `zero-yx/static_flow` 的个人站点分支，目标是在另一台机器上复现当前本地部署方式。

## 1. 克隆个人分支

```bash
git clone --branch personal/site-build https://github.com/zero-yx/static_flow.git
cd static_flow
git submodule update --init --recursive
```

如果你想继续跟踪原作者仓库，可以额外加 upstream：

```bash
git remote add upstream https://github.com/acking-you/static_flow.git
git fetch upstream
```

## 2. 准备基础工具

需要本机已有：

- Rust 和 rustup
- Node.js 和 npm
- GitHub CLI `gh`
- `trunk`

常用安装命令：

```bash
rustup target add wasm32-unknown-unknown
cargo install trunk
gh auth login -h github.com
```

`scripts/build_frontend_selfhosted.sh` 会在缺少前端 npm 依赖时自动执行 `npm install`。

## 3. 配置 GitHub Wrapped

如果只部署仓库里已经生成好的 Wrapped 页面，可以跳过本节。

如果要在新机器上重新生成自己的 GitHub Wrapped，不要把 token 写入仓库。推荐用 GitHub CLI 的 keyring：

```bash
mkdir -p .local
cat > .local/github-wrapped.env <<'EOF'
export GITHUB_WRAPPED_LOGIN=zero-yx
export GH_TOKEN="$(gh auth token)"
EOF

source .local/github-wrapped.env
scripts/github-wrapped.sh --year 2026
scripts/github-wrapped.sh --list-years
```

`.local/` 已经被 `.gitignore` 忽略。`GH_TOKEN` 只会在当前 shell 中展开，不会进入 git 提交。

如果要生成其他年份：

```bash
source .local/github-wrapped.env
scripts/github-wrapped.sh --year 2025
```

生成后需要重新构建前端。

## 4. 构建前端

自托管模式使用同源 API，也就是前端访问 `/api`：

```bash
bash scripts/build_frontend_selfhosted.sh
```

构建产物在：

```text
crates/frontend/dist
```

其中 `crates/frontend/standalone` 下的 Wrapped 页面会被复制到 `crates/frontend/dist/standalone`。

## 5. 初始化数据目录

后端启动时需要 LanceDB 数据目录。新机器可以先用本地目录：

```bash
export DB_ROOT="$PWD/data"
cargo build --release -p sf-cli
target/release/sf-cli init --db-path "$DB_ROOT/lancedb"
mkdir -p "$DB_ROOT/lancedb-comments" "$DB_ROOT/lancedb-music"
```

如果你有自己的生产数据，把 `DB_ROOT` 指向实际数据根目录即可。

## 6. 构建并启动后端

```bash
cargo build --profile release-backend -p static-flow-backend

DB_ROOT="$PWD/data" \
SITE_BASE_URL="http://127.0.0.1:39080" \
FRONTEND_DIST_DIR="$PWD/crates/frontend/dist" \
bash scripts/start_backend_selfhosted.sh
```

打开：

```text
http://127.0.0.1:39080/
```

后台运行可以加 `--daemon`：

```bash
DB_ROOT="$PWD/data" \
SITE_BASE_URL="https://your-domain.example" \
FRONTEND_DIST_DIR="$PWD/crates/frontend/dist" \
bash scripts/start_backend_selfhosted.sh --daemon
```

## 7. 生产部署要点

- `personal/site-build` 用作个人二开部署分支。
- `master` 建议只跟踪原作者 upstream，减少同步冲突。
- 生产域名需要设置 `SITE_BASE_URL=https://your-domain.example`。
- 如果用反向代理，后端默认监听 `127.0.0.1:39080`，代理转发到该端口即可。
- GitHub token 只用于生成 Wrapped 页面，不是后端运行必需项。
- 每次更新 Wrapped 或首页文案后，至少执行：

```bash
bash scripts/test_github_wrapped.sh
cargo check -p static-flow-frontend --target wasm32-unknown-unknown
bash scripts/build_frontend_selfhosted.sh
```

