"""android 命令：Stross Android 工程装配（原 scripts/setup-android.sh 的等价迁移）。

步骤：
  1. cargo tauri android init        —— 生成 gen/android 工程
  2. 复制 Kotlin 插件源码进工程
  3. 替换 MainActivity（注册 MediaPlugin）
  4. 注入 AndroidManifest 权限与前台服务声明
  5. 顺带补齐 Rust Android 目标（已存在则跳过）

JDK 约束（AGENTS.md / docs/android-build.md）：构建必须 JVM ≤ 21（Gradle 8 与
系统 JDK 25 不兼容）；本机默认 JDK 21，若 JAVA_HOME 未设或指向 ≥25 则收敛到
/usr/lib/jvm/java-21-openjdk。

之后构建：
  uv run python -m scripts build android --debug
"""

from __future__ import annotations

import shutil

from .. import util
from ..util import REPO, run

# 从 tauri 工程 android/ 目录复制进 gen 工程的 Kotlin 源（含 MainActivity 替换）
_KOTLIN_SOURCES = ("MediaPlugin.kt", "PlaybackPlugin.kt", "ProjectionService.kt",
                   "MainActivity.kt")

_PERMISSIONS = """    <uses-permission android:name="android.permission.INTERNET" />
    <uses-permission android:name="android.permission.RECORD_AUDIO" />
    <uses-permission android:name="android.permission.FOREGROUND_SERVICE" />
    <uses-permission android:name="android.permission.FOREGROUND_SERVICE_MEDIA_PROJECTION" />
    <uses-permission android:name="android.permission.POST_NOTIFICATIONS" />
    <uses-permission android:name="android.permission.CHANGE_WIFI_MULTICAST_STATE" />
"""

_SERVICE = """        <service
            android:name="dev.stross.sender.ProjectionService"
            android:exported="false"
            android:foregroundServiceType="mediaProjection" />
"""


def _inject_manifest(manifest_path) -> None:
    """AndroidManifest 权限与前台服务注入（等价旧 setup-android.sh 的 python 内嵌段）。"""
    s = manifest_path.read_text(encoding="utf-8")
    if "FOREGROUND_SERVICE_MEDIA_PROJECTION" not in s:
        s = s.replace("<application", _PERMISSIONS + "    <application")
    if "ProjectionService" not in s:
        # 插到 </application> 前
        s = s.replace("</application>", _SERVICE + "    </application>")
    manifest_path.write_text(s, encoding="utf-8")
    print("    AndroidManifest.xml 已更新")


def cmd(args) -> int:
    tauri_dir = REPO / "apps/stross-gui/src-tauri"
    gen = tauri_dir / "gen/android"
    env = util.jdk21_env()

    print("==> 1/4 cargo tauri android init")
    r = run(["cargo", "tauri", "android", "init"], cwd=tauri_dir, env=env)
    if r.returncode != 0:
        return r.returncode  # 等价 set -e：直接失败退出

    pkg_dir = gen / "app/src/main/java/dev/stross/sender"
    print(f"==> 2/4 复制 Kotlin 插件 -> {pkg_dir}")
    pkg_dir.mkdir(parents=True, exist_ok=True)
    for name in _KOTLIN_SOURCES:
        shutil.copy2(tauri_dir / "android" / name, pkg_dir / name)

    print("==> 3/4 注入 AndroidManifest 权限/服务")
    _inject_manifest(gen / "app/src/main/AndroidManifest.xml")

    print("")
    print("==> 4/4 完成 ✅")
    print("")
    print("接下来构建 APK:")
    print("  cd apps/stross-gui/src-tauri")
    print("  cargo tauri android build --apk --debug")
    print("")
    print("构建产物: apps/stross-gui/src-tauri/gen/android/app/build/outputs/apk/")

    # 顺带把 Rust Android 目标补上（已存在则跳过）
    r = run(["rustup", "target", "list", "--installed"], capture=True, env=env)
    if "aarch64-linux-android" not in r.stdout:
        run(["rustup", "target", "add", "aarch64-linux-android",
             "armv7-linux-androideabi", "x86_64-linux-android"], env=env)
    return 0
