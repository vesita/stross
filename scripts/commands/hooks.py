"""hooks 命令：安装/卸载 Stross 本地 pre-commit 钩子（原 scripts/install-hooks.sh）。

钩子在每次 git commit 前自动跑 `uv run python -m scripts check --quick`
（rustfmt + clippy + 前端类型 + app.js 同步，秒级）；失败则阻止提交。
想临时跳过：git commit --no-verify。

用法：
    uv run python -m scripts hooks           安装
    uv run python -m scripts hooks --remove  卸载
"""

from __future__ import annotations

from ..util import REPO

HOOK_REL = ".git/hooks/pre-commit"

_HOOK_TEMPLATE = """#!/usr/bin/env bash
# Stross pre-commit：快速检查（由 uv run python -m scripts hooks 生成，勿手改）
set -uo pipefail
REPO="$(git rev-parse --show-toplevel 2>/dev/null)"
if [ -n "$REPO" ] && [ -f "$REPO/pyproject.toml" ]; then
  if ! (cd "$REPO" && uv run python -m scripts check --quick); then
    echo "✗ 快速检查未通过，已阻止提交（确认无误可用 git commit --no-verify 绕过）" >&2
    exit 1
  fi
fi
"""


def cmd(args) -> int:
    hook = REPO / HOOK_REL
    if getattr(args, "remove", False):
        hook.unlink(missing_ok=True)
        print(f"已卸载 pre-commit 钩子：{hook}")
        return 0

    hook.parent.mkdir(parents=True, exist_ok=True)
    hook.write_text(_HOOK_TEMPLATE, encoding="utf-8")
    hook.chmod(0o755)
    print(f"已安装 pre-commit 钩子（每次提交自动快速检查）：{hook}")
    return 0
