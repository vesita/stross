"""frontend 命令：前端无头测试与产物同步检查。

- test（默认）：jsdom 无头交互测试（原 scripts/test-frontend.mjs）。
  JS 实现主体迁到 scripts/jsdom-test.mjs（前端测试必须跑在 node+jsdom 环境），
  Python 只做「调用 + 断言退出码」。
- sync：tsc 编译 + git diff 检查 app.js 同步（原 scripts/check-frontend.sh）。
"""

from __future__ import annotations

from ..util import REPO, run

TSC_VERSION = "5.9.3"


def _frontend_test() -> int:
    # 等价 `node scripts/test-frontend.mjs`（脚本自身会按需临时安装 jsdom@24）
    r = run(["node", "scripts/jsdom-test.mjs"], cwd=REPO, node_path=True)
    return r.returncode


def _frontend_sync() -> int:
    # 等价 check-frontend.sh：tsc 就地编译 → git diff 检查 app.js 漂移
    for d in ["apps/stross-gui/web"]:
        r = run(["npx", "-y", "-p", f"typescript@{TSC_VERSION}", "tsc",
                 "-p", f"{d}/tsconfig.json", "--pretty", "false"],
                cwd=REPO, node_path=True)
        if r.returncode != 0:
            return r.returncode  # set -e：tsc 失败即退出
        git = run(["git", "diff", "--quiet", "--", f"{d}/app.js"], cwd=REPO,
                  capture=True)
        if git.returncode != 0:
            print(f"✗ {d}/app.js 与 app.ts 不一致 —— 请运行：")
            print(f"    npx -y -p typescript@{TSC_VERSION} tsc -p {d}/tsconfig.json")
            print("  并提交生成的 app.js（.ts 是唯一真源）。")
            return 1
    print("✓ 前端产物与 TypeScript 源一致")
    return 0


def cmd(args) -> int:
    if args.action == "sync":
        return _frontend_sync()
    return _frontend_test()
