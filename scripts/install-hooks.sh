#!/usr/bin/env bash
# 安装 / 卸载 Stross 本地 pre-commit 钩子（不引入远端 CI 工作流）。
#
# 钩子在每次 git commit 前自动跑 scripts/check.sh --quick
# （rustfmt + clippy + 前端类型 + app.js 同步，秒级）；失败则阻止提交。
# 想临时跳过：git commit --no-verify。
#
# 用法：
#   scripts/install-hooks.sh          安装
#   scripts/install-hooks.sh --remove 卸载
set -euo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
HOOK="$REPO/.git/hooks/pre-commit"

if [ "${1:-}" = "--remove" ]; then
  rm -f "$HOOK"
  echo "已卸载 pre-commit 钩子：$HOOK"
  exit 0
fi

mkdir -p "$(dirname "$HOOK")"
cat > "$HOOK" <<'EOF'
#!/usr/bin/env bash
# Stross pre-commit：快速检查（由 scripts/install-hooks.sh 生成，勿手改）
set -uo pipefail
REPO="$(git rev-parse --show-toplevel 2>/dev/null)"
if [ -n "$REPO" ] && [ -x "$REPO/scripts/check.sh" ]; then
  if ! "$REPO/scripts/check.sh" --quick; then
    echo "✗ 快速检查未通过，已阻止提交（确认无误可用 git commit --no-verify 绕过）" >&2
    exit 1
  fi
fi
EOF
chmod +x "$HOOK"
echo "已安装 pre-commit 钩子（每次提交自动快速检查）：$HOOK"
