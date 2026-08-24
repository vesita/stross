#!/usr/bin/env bash
# 前端 TypeScript → JS 漂移检查。
#
# 两个前端的 app.js 是构建产物（提交进仓库）：
#   - 观看端 app.js 被 Rust include_str! 编译期内嵌（crates/stross-core/assets.rs）
#   - 推流端 app.js 被 Tauri index.html 直接加载
# cargo 构建零 node 依赖，因此产物必须与 app.ts 保持同步提交。
#
# 用法（提交前 / CI）：
#   scripts/check-frontend.sh
set -euo pipefail
cd "$(dirname "$0")/.."

TSC_VERSION="5.9.3"

for d in crates/stross-core/assets/viewer apps/stross-gui/web; do
  npx -y -p "typescript@${TSC_VERSION}" tsc -p "$d/tsconfig.json" --pretty false
  if ! git diff --quiet -- "$d/app.js"; then
    echo "✗ $d/app.js 与 app.ts 不一致 —— 请运行："
    echo "    npx -y -p typescript@${TSC_VERSION} tsc -p $d/tsconfig.json"
    echo "  并提交生成的 app.js（.ts 是唯一真源）。"
    exit 1
  fi
done

echo "✓ 前端产物与 TypeScript 源一致"
