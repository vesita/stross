#!/usr/bin/env bash
# Stross 构建自动化（参数化产物构建）。
#
# 用法：
#   scripts/build.sh cli              构建 stross-cli（debug）
#   scripts/build.sh relay            构建独立中继 stross-relay（debug）
#   scripts/build.sh gui              构建桌面 GUI（tauri，debug）
#   scripts/build.sh android          构建 Android APK（需先 scripts/setup-android.sh）
#   任意命令加 --release 用 release 配置
#     * cli/relay：cargo build --release
#     * gui/android：tauri 系默认就是 release（无 --release 选项），
#       debug 用 -d/--debug 显式切换（见 `cargo tauri build --help`）
#
# 产物路径统一输出到 stdout 结尾，供脚本/CI 消费。
# 任一构建命令失败即退出（不打印误导性的「构建完成」）。
set -uo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO"

TARGET="${1:-cli}"
PROFILE="${2:-debug}"
[ "$PROFILE" = "--release" ] && PROFILE=release
REL=""            # cargo（cli/relay）：release 传 --release
TAURI_FLAGS=""    # tauri（gui/android）：默认 release；debug 传 -d
if [ "$PROFILE" = "release" ]; then
  REL="--release"
else
  TAURI_FLAGS="--debug"
fi
OUT=()

fail() { echo "✗ $*" >&2; exit 1; }

case "$TARGET" in
  cli)
    cargo build $REL -p stross-cli || fail "cli 构建失败"
    OUT=("$REPO/target/$PROFILE/stross")
    ;;
  relay)
    cargo build $REL -p stross-relay || fail "relay 构建失败"
    OUT=("$REPO/target/$PROFILE/stross-relay")
    ;;
  gui)
    # tauri build 默认 release；--bundles deb 失败（如缺依赖打包器）时
    # 回退无 bundle 的纯二进制构建
    cargo tauri build $TAURI_FLAGS --bundles deb 2>/dev/null \
      || cargo tauri build $TAURI_FLAGS \
      || fail "gui 构建失败"
    OUT=($(ls -d "$REPO"/target/$PROFILE/bundle/*/ 2>/dev/null))
    ;;
  android)
    [ -f "$REPO/apps/stross-gui/src-tauri/gen/android/settings.gradle" ] \
      || fail "Android 工程未装配：请先运行 scripts/setup-android.sh"
    cargo tauri android build $TAURI_FLAGS || fail "android 构建失败"
    # 只列本次 profile 的 APK（release 含 -release 与 -release-unsigned）
    OUT=($(find "$REPO/apps/stross-gui/src-tauri/gen/android" \
      -name "*.apk" -name "*$PROFILE*" 2>/dev/null))
    ;;
  *)
    echo "未知目标: $TARGET（cli | relay | gui | android）" >&2
    exit 2
    ;;
esac

echo "✅ $TARGET（$PROFILE）构建完成："
for o in "${OUT[@]:-}"; do [ -e "$o" ] && echo "  $o"; done