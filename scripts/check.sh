#!/usr/bin/env bash
# Stross 本地自动化检查入口（"本地 CI"——不引入远端 CI 工作流）。
#
# 用法：
#   scripts/check.sh            全量：fmt + clippy + 单元/集成测试 + 前端（类型/同步/jsdom）
#   scripts/check.sh --quick    提交前快速：fmt + clippy + 前端类型 + app.js 同步（秒级）
#   scripts/check.sh --e2e      双设备端到端（serve 推流 → 直连/中途/级联解码）
#
# 任一环节失败即退出非零，并提示如何修复。
set -uo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO"

PASS=0
step() { printf '\n\033[1;34m== %s ==\033[0m\n' "$*"; }
ok()   { printf '\033[1;32m✓ %s\033[0m\n' "$*"; PASS=$((PASS + 1)); }
fail() { printf '\033[1;31m✗ %s\033[0m\n' "$*"; exit 1; }

MODE="${1:-full}"
# 是否运行重量级步骤（workspace 测试 / jsdom）——quick 模式跳过
case "$MODE" in
  --full|full|--e2e|e2e) RUN_FULL=1 ;;
  *) RUN_FULL=0 ;;
esac

# ---------------------------------------------------------------- Rust
check_rust() {
  step "rustfmt --check"
  cargo fmt --check || fail "cargo fmt 未通过（运行 cargo fmt）"

  step "clippy（-D warnings）"
  cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -1 \
    && ok "clippy" || fail "clippy 有告警（cargo clippy --fix 可自动修）"

  if [ "$RUN_FULL" = "1" ]; then
    step "cargo test --workspace"
    cargo test --workspace 2>&1 | grep -E "test result: FAILED|error\[" && fail "存在失败用例"
    ok "workspace 测试"
  fi
}

# ---------------------------------------------------------------- 前端
check_frontend() {
  local TSC="npx -y -p typescript@5.9.3 tsc"

  step "前端类型检查（tsc strict）"
  $TSC -p apps/stross-gui/web/tsconfig.json --pretty false --noEmit \
    && ok "tsc" || fail "前端类型错误"

  step "app/*.js 与 app/*.ts 同步（编译产物比对，不依赖 git 状态）"
  local tmp
  tmp="$(mktemp -d)"
  $TSC -p apps/stross-gui/web/tsconfig.json --pretty false --outDir "$tmp" > /dev/null 2>&1
  # 多文件产物逐个比对（--outDir 时 tsc 按公共根目录 app/ 平铺输出到 "$tmp"；
  # .ts 是唯一真源，app/*.js 提交进仓库）
  local ok_sync=1
  for f in "$tmp"/*.js; do
    local b
    b="$(basename "$f")"
    if ! cmp -s "$f" "apps/stross-gui/web/app/$b"; then
      ok_sync=0
      echo "  app/$b 与源不同步" >&2
    fi
  done
  if [ "$ok_sync" = "1" ]; then
    ok "app/*.js 同步"
  else
    fail "app/*.js 与 app/*.ts 不一致：请运行 tsc 重新生成（.ts 是唯一真源）"
  fi
  rm -rf "$tmp"

  if [ "$RUN_FULL" = "1" ]; then
    step "前端交互无头测试（jsdom）"
    node scripts/test-frontend.mjs > /dev/null 2>&1 \
      && ok "jsdom" || fail "前端无头测试失败（node scripts/test-frontend.mjs 看详情）"
  fi
}

# ---------------------------------------------------------------- 端到端
check_e2e() {
  step "双设备端到端（直连 / 中途接入 / 级联代理）"
  scripts/dual-device-test.sh > /dev/null 2>&1 \
    && ok "双设备 e2e" || fail "双设备 e2e 失败（scripts/dual-device-test.sh 看详情）"
}

case "$MODE" in
  --quick|quick) check_rust; check_frontend ;;
  --e2e|e2e)     check_rust; check_frontend; check_e2e ;;
  --full|full)   check_rust; check_frontend ;;
  *) echo "未知模式: $MODE（full | quick | e2e）" >&2; exit 2 ;;
esac

printf '\n\033[1;32m✅ %s 检查全部通过（%d 项）\033[0m\n' "$MODE" "$PASS"
