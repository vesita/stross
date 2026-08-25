#!/usr/bin/env bash
# Stross 构建自动化（参数化产物构建）。
#
# 用法：
#   scripts/build.sh cli              构建 stross-cli（debug）
#   scripts/build.sh relay            构建独立中继 stross-relay（debug）
#   scripts/build.sh gui              构建桌面 GUI（tauri，debug）
#   scripts/build.sh android          构建 Android APK（需先 scripts/setup-android.sh）
#   任意命令加 --release 用 release 配置
#
# 产物路径统一输出到 stdout 结尾，供脚本/CI 消费。
set -uo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO"

TARGET="${1:-cli}"
PROFILE="${2:-debug}"
[ "$PROFILE" = "--release" ] && { PROFILE=release; REL="--release"; } || REL=""
OUT=()

case "$TARGET" in
  cli)
    cargo build $REL -p stross-cli
    OUT=("$REPO/target/$PROFILE/stross")
    ;;
  relay)
    cargo build $REL -p stross-relay
    OUT=("$REPO/target/$PROFILE/stross-relay")
    ;;
  gui)
    cargo tauri build $REL --bundles deb 2>/dev/null || cargo tauri build $REL
    OUT=($(ls -d "$REPO"/target/$PROFILE/bundle/*/ 2>/dev/null))
    ;;
  android)
    [ -f "$REPO/apps/stross-gui/src-tauri/gen/android/settings.gradle" ] \
      || { echo "✗ Android 工程未装配：请先运行 scripts/setup-android.sh" >&2; exit 1; }
    cargo tauri android build $REL
    OUT=($(find "$REPO/apps/stross-gui/src-tauri/gen/android" -name "*.apk" 2>/dev/null))
    ;;
  *)
    echo "未知目标: $TARGET（cli | relay | gui | android）" >&2
    exit 2
    ;;
esac

echo "✅ $TARGET（$PROFILE）构建完成："
for o in "${OUT[@]:-}"; do [ -e "$o" ] && echo "  $o"; done
