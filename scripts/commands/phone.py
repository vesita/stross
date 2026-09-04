"""phone 命令：手机 WebView CDP 驱动（原 scripts/phone-cdp.mjs 的 Python 封装）。

JS 实现主体保留在 scripts/phone-cdp.mjs（前端必须跑在 node+jsdom/CDP 环境），
Python 只做「透传子命令 + 断言退出码」。等价：
    node scripts/phone-cdp.mjs dump | text | click '<sel>' | eval '<js>'
环境变量 CDP_PORT 控制调试端口（默认 19222），与旧脚本一致。
"""

from __future__ import annotations

from ..util import REPO, run


def cmd(args) -> int:
    cmd = ["node", str(REPO / "scripts/phone-cdp.mjs"), args.action]
    if args.arg is not None:
        cmd.append(args.arg)
    r = run(cmd, cwd=REPO, node_path=True)
    return r.returncode
