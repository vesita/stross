#!/usr/bin/env bash
# Stross 双 PC 流稳定性 + 端到端延迟测试（B4）。
#
# 拓扑（本地双开，同一时钟，绝对延迟可测）：
#   PC-A（接收端）: stross serve（内核 + 受控中继 + 控制面）→ 建会话 → 签发凭证
#   PC-B（推流端）: stross push --share-token（视频 + 440Hz 测试音）→ --report-start 记录会话起点
#   PC-A（接收端）: stross receive --latency --calibrate → 绝对端到端延迟 + 相对抖动
#
# 每轮一个传输（默认 srt + quic）：
#   * 稳定性：长跑接收帧数/音频块数 vs 期望（30fps、AAC ~47 块/s）、serve 进程 RSS 有界
#   * 延迟：绝对端到端 min/p50/p95/p99（同钟校准，含 ffmpeg 预热正偏差=上界）、
#     相对附加延迟（传输/缓冲抖动）
#
# 用法：scripts/latency-stability-test.sh [SECS] [TRANS...]
#   例：scripts/latency-stability-test.sh 120 srt quic ws
# 退出码：0 = 每轮帧数/音频块 ≥ 阈值 且 绝对延迟 min 与 p99 差距 ≤ 抖动上限。
set -uo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
CLI="${CLI:-$REPO/target/debug/stross}"
OUT="${OUT:-/tmp/stross-latency}"
PORT=18777
CTRL=18778
SECS="${1:-60}"
TRANS="${2:-srt quic}"
MIN_FRAMES_PCT=90        # 接收帧数 ≥ 90% 期望
MIN_AUDIO_PCT=90         # 音频块 ≥ 90% 期望
MAX_TAIL_JITTER=250       # abs p99 − min ≤ 250ms（传输/缓冲尾延迟上界）

LAN_IP=$(ip -4 -o addr show scope global 2>/dev/null | awk '{print $4}' | cut -d/ -f1 | head -1)
[ -n "$LAN_IP" ] || LAN_IP=$(hostname -I 2>/dev/null | awk '{print $1}')
[ -n "$LAN_IP" ] || LAN_IP=127.0.0.1

cargo build -p stross-cli >/dev/null 2>&1 || { echo "✗ 构建失败"; exit 1; }
rm -rf "$OUT" && mkdir -p "$OUT"
log() { printf '\n\033[1;34m== %s ==\033[0m\n' "$*"; }

cleanup() { kill "${A_PID:-}" 2>/dev/null || true; kill "${B_PID:-}" 2>/dev/null || true; }
trap cleanup EXIT

EXPECT_FRAMES=$((SECS * 30))
EXPECT_AUDIO=$((SECS * 47))

run_round() {
  local trans=$1
  log "传输 $trans（${SECS}s 长跑）— 期望 ${EXPECT_FRAMES} 帧 / ${EXPECT_AUDIO} 音频块"
  rm -rf "$OUT/$trans" && mkdir -p "$OUT/$trans"

  "$CLI" serve --port "$PORT" --ctrl-port "$CTRL" > "$OUT/$trans/serve.log" 2>&1 &
  A_PID=$!
  sleep 1.2

  SID=$("$CLI" ctrl create-session --title 稳定性测试 2>&1 | grep -o 'sessionId: [a-z0-9-]*' | awk '{print $2}')
  [ -n "$SID" ] || { echo "✗ 建会话失败"; return 1; }
  TOKEN=$("$CLI" ctrl share-token "$SID" --ttl 600 2>&1 | grep -oE '\{"v":[0-9].*' | head -1)
  [ -n "$TOKEN" ] || { echo "✗ 签发凭证失败"; return 1; }

  # 推流地址按传输取 /api/info
  INFO=$(curl -s --max-time 2 "http://127.0.0.1:$PORT/api/info")
  case "$trans" in
    srt) PUSH_URL="srt://$LAN_IP:$(echo "$INFO" | grep -oE '"srtPort":[0-9]+' | cut -d: -f2)"; WATCH_URL="srt://$LAN_IP:$(echo "$INFO" | grep -oE '"srtPort":[0-9]+' | cut -d: -f2)";;
    quic) PUSH_URL="quic://$LAN_IP:$(echo "$INFO" | grep -oE '"quicPort":[0-9]+' | cut -d: -f2)"; WATCH_URL="quic://$LAN_IP:$(echo "$INFO" | grep -oE '"quicPort":[0-9]+' | cut -d: -f2)";;
    ws) PUSH_URL="ws://$LAN_IP:$PORT/ws/push"; WATCH_URL="ws://$LAN_IP:$PORT";;
    *) echo "未知传输 $trans"; return 1;;
  esac

  RSS0=$(ps -o rss= -p "$A_PID" 2>/dev/null | tr -d ' ')

  # PC-B 推流（后台；凭证接入，Welcome 后才返回）
  "$CLI" push --relay "$PUSH_URL" --stream-id "$SID" --share-token "$TOKEN" \
    --secs "$SECS" --audio --report-start "$OUT/$trans/start.json" \
    > "$OUT/$trans/push.log" 2>&1 &
  B_PID=$!
  sleep 1.5
  grep -q "中继已确认推流" "$OUT/$trans/push.log" || { echo "  ❌ B 端接入失败"; tail -3 "$OUT/$trans/push.log"; return 1; }

  # PC-A 长跑接收（延迟 + 稳定性采样；--no-write 不落盘，避免 tmpfs 写满）
  "$CLI" receive --relay "$WATCH_URL" --stream "$SID" --secs "$SECS" --latency --no-write \
    --calibrate "$OUT/$trans/start.json" --out "$OUT/$trans/recv" \
    > "$OUT/$trans/recv.log" 2>&1
  wait "$B_PID" 2>/dev/null

  # 统计
  FRAMES=$(grep -oE "解码视频 [0-9]+" "$OUT/$trans/recv.log" | grep -oE "[0-9]+" | head -1)
  FRAMES=${FRAMES:-0}
  AUDIO=$(grep -oE "音频块 [0-9]+" "$OUT/$trans/recv.log" | grep -oE "[0-9]+" | head -1)
  AUDIO=${AUDIO:-0}
  ABS_MIN=$(grep "绝对端到端延迟" "$OUT/$trans/recv.log" | grep -oE "min=[0-9.]+" | grep -oE "[0-9.]+" | head -1)
  ABS_P99=$(grep "绝对端到端延迟" "$OUT/$trans/recv.log" | grep -oE "p99=[0-9.]+" | grep -oE "[0-9.]+$" | head -1)
  REL=$(grep -oE "p50=[-0-9.]+ p90=[-0-9.]+ p95=[-0-9.]+ p99=[-0-9.]+ max=[-0-9.]+" "$OUT/$trans/recv.log" | head -1)
  RSS1=$(ps -o rss= -p "$A_PID" 2>/dev/null | tr -d ' ')
  kill "$A_PID" 2>/dev/null; wait "$A_PID" 2>/dev/null; A_PID=""
  kill "$B_PID" 2>/dev/null; wait "$B_PID" 2>/dev/null; B_PID=""
  rm -rf "$OUT/$trans" "$OUT"/start.json 2>/dev/null""

  echo "  接收: ${FRAMES}/${EXPECT_FRAMES} 帧 | 音频块 ${AUDIO}/${EXPECT_AUDIO}"
  [ -n "$ABS_MIN" ] && echo "  绝对端到端延迟 ms: min=${ABS_MIN} p99=${ABS_P99:-?}（含 ffmpeg 预热上界）"
  [ -n "$REL" ] && echo "  相对附加延迟（抖动/尾延迟）: $REL"
  [ -n "$RSS0" ] && [ -n "$RSS1" ] && echo "  serve RSS: ${RSS0} KB → ${RSS1} KB"

  # 断言
  local ok=1
  [ "$FRAMES" -ge $((EXPECT_FRAMES * MIN_FRAMES_PCT / 100)) ] || { echo "  ❌ 视频帧不足"; ok=0; }
  [ "$AUDIO" -ge $((EXPECT_AUDIO * MIN_AUDIO_PCT / 100)) ] || { echo "  ❌ 音频块不足"; ok=0; }
  if [ -n "$ABS_MIN" ] && [ -n "$ABS_P99" ]; then
    awk -v a="$ABS_MIN" -v b="$ABS_P99" -v t="$MAX_TAIL_JITTER" \
      'BEGIN { if (b - a > t) { print "  ❌ 尾延迟超限 (p99−min=" b-a "ms > " t "ms)"; exit 1 } }' \
      || ok=0
  fi
  [ "$ok" -eq 1 ] && echo "  ✅ $trans 稳定 + 延迟达标" || { echo "  ❌ $trans 未达标"; return 1; }
}

FAILED=0
for t in $TRANS; do
  run_round "$t" || FAILED=1
done
trap - EXIT
[ "$FAILED" -eq 0 ] && echo "✅ 双 PC 稳定性 + 延迟全部达标" || { echo "❌ 存在未达标项"; exit 1; }
