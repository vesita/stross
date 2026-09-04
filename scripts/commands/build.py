"""build 命令：参数化产物构建（原 scripts/build.sh 的机械等价迁移）。

用法：
    uv run python -m scripts build cli              构建 stross-cli（debug）
    uv run python -m scripts build relay            构建 stross-relay（debug）
    uv run python -m scripts build gui              构建桌面 GUI（tauri，release 默认）
    uv run python -m scripts build android          构建 Android APK（需先 setup-android）
    任意目标加 --release 用 release 配置；gui/android 的 debug 用 --debug。
产物路径统一输出到 stdout 结尾，供脚本/CI 消费；任一构建命令失败即退出非零。
"""

from __future__ import annotations

import glob
import os
import subprocess
import sys
from pathlib import Path

from .. import util
from ..util import REPO, run

TARGETS = ("cli", "relay", "gui", "android")


def _fail(msg: str) -> None:
    # 等价 build.sh 的 fail()：✗ 前缀输出到 stderr，退出码 1
    print(f"✗ {msg}", file=sys.stderr)
    raise SystemExit(1)


def cmd(args) -> int:
    target = args.target
    profile = args.profile or "debug"
    if args.release:
        profile = "release"
    if args.debug:
        profile = "debug"
    if profile == "--release":  # 旧脚本第二个位置参数也接受字面 "--release"
        profile = "release"

    rel = ["--release"] if profile == "release" else []
    tauri_flags = [] if profile == "release" else ["--debug"]

    out: list[str] = []

    if target == "cli":
        r = run(["cargo", "build", *rel, "-p", "stross-cli"])
        if r.returncode != 0:
            _fail("cli 构建失败")
        out = [str(REPO / f"target/{profile}/stross")]

    elif target == "relay":
        r = run(["cargo", "build", *rel, "-p", "stross-relay"])
        if r.returncode != 0:
            _fail("relay 构建失败")
        out = [str(REPO / f"target/{profile}/stross-relay")]

    elif target == "gui":
        # tauri build 默认 release；--bundles deb 失败（如缺依赖打包器）时
        # 回退无 bundle 的纯二进制构建（等价 build.sh 的 `2>/dev/null ||` 链）
        r = run(["cargo", "tauri", "build", *tauri_flags, "--bundles", "deb"],
                stderr=subprocess.DEVNULL)
        if r.returncode != 0:
            r = run(["cargo", "tauri", "build", *tauri_flags])
            if r.returncode != 0:
                _fail("gui 构建失败")
        out = [d for d in glob.glob(str(REPO / f"target/{profile}/bundle/*/"))]

    elif target == "android":
        if not (REPO / "apps/stross-gui/src-tauri/gen/android/settings.gradle").exists():
            _fail("Android 工程未装配：请先运行 uv run python -m scripts android")
        env = util.jdk21_env()
        r = run(["cargo", "tauri", "android", "build", *tauri_flags], env=env)
        if r.returncode != 0:
            _fail("android 构建失败")
        # 只列本次 profile 的 APK（release 含 -release 与 -release-unsigned）
        gen = REPO / "apps/stross-gui/src-tauri/gen/android"
        out = [str(p) for p in gen.rglob("*.apk") if profile in p.name]

    else:
        print(f"未知目标: {target}（cli | relay | gui | android）", file=sys.stderr)
        return 2

    print(f"✅ {target}（{profile}）构建完成：")
    for o in out:
        if os.path.exists(o):
            print(f"  {o}")
    return 0
