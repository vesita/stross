#!/usr/bin/env bash
# =============================================================================
# Stross Android 工程装配脚本
#
# 用法:
#   ./scripts/setup-android.sh
#
# 步骤:
#   1. cargo tauri android init        —— 生成 gen/android 工程
#   2. 复制 Kotlin 插件源码进工程
#   3. 替换 MainActivity（注册 MediaPlugin）
#   4. 注入 AndroidManifest 权限与前台服务声明
#
# 之后构建:
#   cd apps/stross-sender/src-tauri
#   cargo tauri android build --apk        # 或 --apk --debug
#
# 前置条件: Android SDK + NDK、JDK 17+、Rust Android 目标
#   rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
# =============================================================================
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TAURI_DIR="$ROOT/apps/stross-sender/src-tauri"
GEN="$TAURI_DIR/gen/android"

echo "==> 1/4 cargo tauri android init"
(cd "$TAURI_DIR" && cargo tauri android init)

PKG_DIR="$GEN/app/src/main/java/dev/stross/sender"
echo "==> 2/4 复制 Kotlin 插件 -> $PKG_DIR"
mkdir -p "$PKG_DIR"
cp "$TAURI_DIR/android/MediaPlugin.kt" "$PKG_DIR/"
cp "$TAURI_DIR/android/ProjectionService.kt" "$PKG_DIR/"
cp "$TAURI_DIR/android/MainActivity.kt" "$PKG_DIR/"

echo "==> 3/4 注入 AndroidManifest 权限/服务"
MANIFEST="$GEN/app/src/main/AndroidManifest.xml"
python3 - "$MANIFEST" << 'PYEOF'
import sys

manifest = sys.argv[1]
with open(manifest, encoding="utf-8") as f:
    s = f.read()

permissions = """    <uses-permission android:name="android.permission.INTERNET" />
    <uses-permission android:name="android.permission.RECORD_AUDIO" />
    <uses-permission android:name="android.permission.FOREGROUND_SERVICE" />
    <uses-permission android:name="android.permission.FOREGROUND_SERVICE_MEDIA_PROJECTION" />
    <uses-permission android:name="android.permission.POST_NOTIFICATIONS" />
    <uses-permission android:name="android.permission.CHANGE_WIFI_MULTICAST_STATE" />
"""
if "FOREGROUND_SERVICE_MEDIA_PROJECTION" not in s:
    s = s.replace("<application", permissions + "    <application")

service = """        <service
            android:name="dev.stross.sender.ProjectionService"
            android:exported="false"
            android:foregroundServiceType="mediaProjection" />
"""
if "ProjectionService" not in s:
    # 插到 </application> 前
    s = s.replace("</application>", service + "    </application>")

with open(manifest, "w", encoding="utf-8") as f:
    f.write(s)
print("    AndroidManifest.xml 已更新")
PYEOF

echo ""
echo "==> 4/4 完成 ✅"
echo ""
echo "接下来构建 APK:"
echo "  cd apps/stross-sender/src-tauri"
echo "  cargo tauri android build --apk --debug"
echo ""
echo "构建产物: apps/stross-sender/src-tauri/gen/android/app/build/outputs/apk/"

# 顺带把 Rust Android 目标补上（已存在则跳过）
rustup target list --installed | grep -q aarch64-linux-android || \
    rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android || true
