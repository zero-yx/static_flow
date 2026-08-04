# 个人 fork 部署说明

这份说明面向 `zero-yx/static_flow` 的个人站点分支，目标是在另一台机器上复现当前本地部署方式。

## 1. 克隆个人分支

```bash
git clone --branch personal/site-build https://github.com/zero-yx/static_flow.git
cd static_flow
```

如果你想继续跟踪原作者仓库，可以额外加 upstream：

```bash
git remote add upstream https://github.com/acking-you/static_flow.git
git fetch upstream
```

这个 fork 目前有两类部署路径：

- 前端/Pages 部署：不需要拉取原作者的私有子模块，GitHub Actions 会临时裁剪成 frontend-only workspace。
- 完整后端自托管：需要 `deps/` 和 `patches/` 下的 path/submodule 依赖可访问。

如果要完整后端自托管，再执行：

```bash
git submodule update --init --recursive
```

如果这里出现 `Repository not found`，说明当前账号无法访问原作者的子模块仓库。解决方式是让原作者授权，或者把这些子模块 mirror 到你自己的可访问仓库后更新 `.gitmodules`。

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

如果当前机器没有子模块访问权，并且你只需要构建前端，可以在一次性部署 checkout 中先裁剪 workspace：

```bash
bash scripts/prepare_frontend_only_workspace.sh --force
bash scripts/build_frontend_selfhosted.sh
```

这个命令会修改当前 checkout 的 `Cargo.toml`，适合 CI 或一次性部署目录，不适合在需要继续开发/同步 upstream 的工作区里提交。

构建产物在：

```text
crates/frontend/dist
```

其中 `crates/frontend/standalone` 下的 Wrapped 页面会被复制到 `crates/frontend/dist/standalone`。

## 5. 初始化数据目录

本节只适用于完整后端自托管。它要求子模块/path 依赖已经可访问。

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
- push 到 `personal/site-build` 会触发 `.github/workflows/deploy.yml` 的前端部署 workflow。
- 该 workflow 不拉取私有子模块，而是在 runner 中临时裁剪 frontend-only workspace。
- workflow 的发布目标是 `zero-yx/zero-yx.github.io` 的 `master` 分支。
- 首次使用前，需要在 `zero-yx/static_flow` 配置 repository secret：`PERSONAL_ACCESS_TOKEN`。
- 生产域名需要设置 `SITE_BASE_URL=https://your-domain.example`。
- 如果用反向代理，后端默认监听 `127.0.0.1:39080`，代理转发到该端口即可。
- GitHub token 只用于生成 Wrapped 页面，不是后端运行必需项。
- 每次更新 Wrapped 或首页文案后，至少执行：

```bash
bash scripts/test_github_wrapped.sh
cargo check -p static-flow-frontend --target wasm32-unknown-unknown
bash scripts/build_frontend_selfhosted.sh
```

## 8. 配置 GitHub Pages 发布 secret

`peaceiris/actions-gh-pages` 要把构建产物推到另一个仓库 `zero-yx/zero-yx.github.io`，所以 `zero-yx/static_flow` 自带的 `GITHUB_TOKEN` 不够用，需要一个能写入 Pages 仓库的 token。

推荐创建 fine-grained PAT，权限尽量限制为：

- Repository access: `zero-yx/zero-yx.github.io`
- Contents: Read and write

然后在本机用 stdin 设置 secret，避免 token 出现在命令历史里：

```bash
read -r -s PERSONAL_ACCESS_TOKEN
printf '%s' "$PERSONAL_ACCESS_TOKEN" | gh secret set PERSONAL_ACCESS_TOKEN --repo zero-yx/static_flow --body-file -
unset PERSONAL_ACCESS_TOKEN
```

如果你决定复用当前 GitHub CLI 登录 token，也可以这样设置：

```bash
gh auth token | gh secret set PERSONAL_ACCESS_TOKEN --repo zero-yx/static_flow --body-file -
```

设置后手动重跑部署：

```bash
gh workflow run deploy.yml --repo zero-yx/static_flow --ref personal/site-build
gh run list --repo zero-yx/static_flow --branch personal/site-build --limit 3
```
