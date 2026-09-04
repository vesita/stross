"""check 命令：本地自动化检查入口（原 scripts/check.sh 的机械等价迁移）。

用法：
    uv run python -m scripts check            全量：fmt + clippy + 单元/集成测试 +
                                              前端（类型/同步/jsdom）
    uv run python -m scripts check --quick    提交前快速：fmt + clippy + 前端类型 +
                                              app.js 同步（秒级）
    uv run python -m scripts check --e2e      双设备端到端（serve 推流 → 直连/中途/级联解码）
任一环节失败即退出非零，并提示如何修复。
"""

from __future__ import annotations

import re
import subprocess
import sys
import tempfile
from pathlib import Path

from .. import util
from ..util import REPO, TestFailure, run, step

TSC = ["npx", "-y", "-p", "typescript@5.9.3", "tsc"]
TSCONFIG = "apps/stross-gui/web/tsconfig.json"

_PASS = {"n": 0}


def _ok(msg: str) -> None:
    print(f"\033[1;32m✓ {msg}\033[0m")
    _PASS["n"] += 1


def _fail(msg: str) -> None:
    print(f"\033[1;31m✗ {msg}\033[0m")
    raise TestFailure(msg)


# ---------------------------------------------------------------- Rust

def check_rust(run_full: bool) -> None:
    step("rustfmt --check")
    r = run(["cargo", "fmt", "--check"], capture=True)
    if r.returncode != 0:
        _fail("cargo fmt 未通过（运行 cargo fmt）")

    step("clippy（-D warnings）")
    r = run(["cargo", "clippy", "--workspace", "--all-targets", "--", "-D", "warnings"],
            capture=True)
    out = r.stdout + r.stderr
    lines = [ln for ln in out.splitlines() if ln.strip()]
    if lines:  # 等价 `2>&1 | tail -1` 把最后一行输出到终端
        print(lines[-1])
    if r.returncode != 0:
        _fail("clippy 有告警（cargo clippy --fix 可自动修）")
    _ok("clippy")

    if run_full:
        step("cargo test --workspace")
        r = run(["cargo", "test", "--workspace"], capture=True)
        if re.search(r"test result: FAILED|error\[", r.stdout + r.stderr):
            _fail("存在失败用例")
        _ok("workspace 测试")


# ---------------------------------------------------------------- 前端

def check_frontend(run_full: bool) -> None:
    step("前端类型检查（tsc strict）")
    r = run([*TSC, "-p", TSCONFIG, "--pretty", "false", "--noEmit"], capture=True,
            node_path=True)
    if r.returncode != 0:
        _fail("前端类型错误")
    _ok("tsc")

    step("app/*.js 与 app/*.ts 同步（编译产物比对，不依赖 git 状态）")
    with tempfile.TemporaryDirectory(prefix="stross-tsc-") as tmp:
        run([*TSC, "-p", TSCONFIG, "--pretty", "false", "--outDir", tmp],
            capture=True, node_path=True)  # 等价 `> /dev/null 2>&1`
        # --outDir 时 tsc 按公共根目录 app/ 平铺输出到 tmp；.ts 是唯一真源，
        # app/*.js 提交进仓库。逐个比对（等价 for f in "$tmp"/*.js; cmp）
        files = sorted(Path(tmp).glob("*.js"))
        ok_sync = True
        if not files:  # tsc 失败/无产物时 bash 的 glob 不展开 → cmp 必败
            ok_sync = False
        for f in files:
            b = f.name
            target = REPO / "apps/stross-gui/web/app" / b
            if not target.exists() or f.read_bytes() != target.read_bytes():
                ok_sync = False
                print(f"  app/{b} 与源不同步", file=sys.stderr)
        if ok_sync:
            _ok("app/*.js 同步")
        else:
            _fail("app/*.js 与 app/*.ts 不一致：请运行 tsc 重新生成（.ts 是唯一真源）")

    if run_full:
        step("前端交互无头测试（jsdom）")
        r = run(["node", "scripts/jsdom-test.mjs"], capture=True, node_path=True)
        if r.returncode == 0:
            _ok("jsdom")
        else:
            _fail("前端无头测试失败（uv run python -m scripts frontend 看详情）")


# ---------------------------------------------------------------- 端到端

def check_e2e() -> None:
    step("双设备端到端（直连 / 中途接入 / 级联代理）")
    r = run([sys.executable, "-m", "scripts", "test-e2e", "dual-device"],
            capture=True)
    if r.returncode == 0:
        _ok("双设备 e2e")
    else:
        _fail("双设备 e2e 失败（uv run python -m scripts test-e2e dual-device 看详情）")


# ---------------------------------------------------------------- 入口

def cmd(args) -> int:
    # 模式归一化：--quick/--full/--e2e 标志优先，其次位置参数（含 "--" 前缀）
    if args.quick:
        mode, display = "quick", "--quick"
    elif args.full:
        mode, display = "full", "--full"
    elif args.e2e:
        mode, display = "e2e", "--e2e"
    else:
        display = args.mode or "full"
        mode = display.lstrip("-")

    if mode not in ("full", "quick", "e2e"):
        print(f"未知模式: {display}（full | quick | e2e）", file=sys.stderr)
        return 2

    run_full = mode in ("full", "e2e")
    try:
        if mode == "quick":
            check_rust(False)
            check_frontend(False)
        elif mode == "e2e":
            check_rust(True)
            check_frontend(True)
            check_e2e()
        else:  # full
            check_rust(True)
            check_frontend(True)
    except TestFailure:
        return 1

    print(f"\n\033[1;32m✅ {display} 检查全部通过（{_PASS['n']} 项）\033[0m")
    return 0
