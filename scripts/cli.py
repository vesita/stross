"""Stross 工具链 CLI 分发（等价旧 scripts/*.sh 与 *.mjs 的单一入口）。

用法：
    uv run python -m scripts --help
    uv run python -m scripts check [--quick|--full|--e2e]
    uv run python -m scripts build <cli|relay|gui|android> [--release|--debug]
    uv run python -m scripts test-e2e <TEST> [args...]   （别名 e2e）
    uv run python -m scripts phone <dump|text|click|eval> [arg]
    uv run python -m scripts frontend [test|sync]
    uv run python -m scripts android
    uv run python -m scripts hooks [--remove]
"""

from __future__ import annotations

import argparse
import sys

from .commands import android, build, check, frontend, hooks, phone, test_e2e

SUBCOMMANDS = ("build", "check", "test-e2e", "phone", "frontend", "android", "hooks")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="stross-tools",
        description="Stross 自动化工具链（uv 管理；等价旧 scripts/*.sh 与 *.mjs，"
                    "见 docs/framework-v3.md §6）",
    )
    sub = parser.add_subparsers(dest="command", metavar="COMMAND")

    p = sub.add_parser("build", help="构建 cli/relay/gui/android（原 build.sh）")
    p.add_argument("target", choices=["cli", "relay", "gui", "android"])
    p.add_argument("profile", nargs="?", default="debug",
                   help="debug（默认）| release（等价旧 build.sh 的第二个参数）")
    p.add_argument("--release", action="store_true", help="release 配置（等价 build.sh … --release）")
    p.add_argument("--debug", action="store_true", help="debug 配置（tauri 系默认 release，显式切 debug）")
    p.set_defaults(handler=build.cmd)

    p = sub.add_parser("check", help="本地 CI 门禁（原 check.sh）")
    p.add_argument("mode", nargs="?", default="full",
                   help="full（默认）| quick | e2e（等价旧 check.sh 的 $1）")
    p.add_argument("--quick", action="store_true", help="提交前快速：fmt+clippy+tsc+app.js 同步")
    p.add_argument("--full", action="store_true", help="全量：加 cargo test + jsdom")
    p.add_argument("--e2e", action="store_true", help="全量 + 双设备端到端")
    p.set_defaults(handler=check.cmd)

    p = sub.add_parser("test-e2e", aliases=["e2e"],
                       help="回归测试（原各 *_test.sh）")
    test_e2e.add_subparsers(p)
    p.set_defaults(handler=test_e2e.cmd)

    p = sub.add_parser("phone", help="手机 WebView CDP 驱动（原 phone-cdp.mjs）")
    p.add_argument("action", choices=["dump", "text", "click", "eval"])
    p.add_argument("arg", nargs="?", default=None, help="click 的选择器或 eval 的 JS 表达式")
    p.set_defaults(handler=phone.cmd)

    p = sub.add_parser("frontend", help="前端无头测试与产物同步检查")
    p.add_argument("action", nargs="?", choices=["test", "sync"], default="test",
                   help="test（默认，jsdom 无头交互，原 test-frontend.mjs）| "
                        "sync（tsc+git diff，原 check-frontend.sh）")
    p.set_defaults(handler=frontend.cmd)

    p = sub.add_parser("android", help="Android 工程装配（原 setup-android.sh）")
    p.set_defaults(handler=android.cmd)

    p = sub.add_parser("hooks", help="安装/卸载 pre-commit 钩子（原 install-hooks.sh）")
    p.add_argument("--remove", action="store_true", help="卸载钩子（等价 install-hooks.sh --remove）")
    p.set_defaults(handler=hooks.cmd)

    return parser


def main(argv=None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    handler = getattr(args, "handler", None)
    if handler is None:
        parser.print_help()
        return 2
    try:
        return handler(args) or 0
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":
    sys.exit(main())
