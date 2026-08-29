#!/usr/bin/env bash
# Stross 本地双设备端到端验证（P0 免先连网格的支撑链路，无 GUI 可跑）。
#
# 设备 A = stross serve（内核 + 受控中继 + 控制面）推合成流；
# 设备 B = 模拟 GUI 网格：/api/info + /api/streams 聚合 → 直连观看解码；
# 中继 C = 独立 stross relay，经 /api/proxy 级联拉 A 的流（跨设备级联观看）；
# 中途接入 = 推流中段才加入的观看者（依赖"每个关键帧含 SPS/PPS"，曾修 bug）。
#
# 用法：
#   scripts/dual-device-test.sh
#
# 退出码：0 = 直连 + 级联 + 中途接入全部 ≥ 阈值帧数。
set -uo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
CLI="${CLI:-$REPO/target/debug/stross}"
OUT="${OUT:-/tmp/stross-dual}"
PORT="${PORT:-18777}"
CTRL="${CTRL:-18778}"
RELAY_C="${RELAY_C:-19003}"
SECS=24          # 推流时长（覆盖直连+中途+级联三段接收窗口）
RECV_SECS=3      # 每次接收时长
MIN_FRAMES=15    # 每段视频解码帧数阈值
MIN_AUDIO=40     # 每段音频块阈值（合成测试音 440Hz，AAC ~47 块/秒）

[ -x "$CLI" ] || cargo build -p stross-cli
rm -rf "$OUT" && mkdir -p "$OUT"
log() { printf '\n\033[1;34m== %s ==\033[0m\n' "$*"; }

cleanup() { kill "${A_PID:-}" 2>/dev/null || true; kill "${C_PID:-}" 2>/dev/null || true; }
trap cleanup EXIT

log "设备 A：serve（内核+受控中继+控制面，端口 $PORT）"
"$CLI" serve --port "$PORT" --ctrl-port "$CTRL" > "$OUT/serve.log" 2>&1 &
A_PID=$!
sleep 1.2

SID=$("$CLI" ctrl create-session --title 双设备测试 2>&1 | grep -o 'sessionId: [a-z0-9-]*' | awk '{print $2}')
[ -n "$SID" ] || { echo "✗ 建会话失败"; exit 1; }
log "会话（内核签发，D4）: $SID"
"$CLI" ctrl start-stream --stream-id "$SID" --secs "$SECS" --audio > /dev/null
sleep 1

log "设备 B 网格聚合：/api/info + /api/streams"
echo "  /api/info → $(curl -s --max-time 2 http://127.0.0.1:$PORT/api/info)"
echo "  /api/streams → $(curl -s --max-time 2 http://127.0.0.1:$PORT/api/streams | head -c 220)"

log "设备 B 点流即看：直连 A 锚点（推流早期接入）"
"$CLI" receive --relay "ws://127.0.0.1:$PORT" --stream "$SID" --out "$OUT/direct" --secs "$RECV_SECS" > "$OUT/direct.log" 2>&1
DIRECT=$(ls "$OUT"/direct/frame_*.rgba 2>/dev/null | wc -l)
DIRECT_AUDIO=$(grep -oE "音频块 [0-9]+" "$OUT/direct.log" | grep -oE "[0-9]+" | head -1)
DIRECT_AUDIO=${DIRECT_AUDIO:-0}
echo "直连解码帧数: $DIRECT | 音频块: $DIRECT_AUDIO（合成测试音 440Hz，AAC）"

log "中途接入（错过首帧，依赖关键帧自带 SPS/PPS）"
sleep 2  # 推流中段
"$CLI" receive --relay "ws://127.0.0.1:$PORT" --stream "$SID" --out "$OUT/late" --secs "$RECV_SECS" > "$OUT/late.log" 2>&1
LATE=$(ls "$OUT"/late/frame_*.rgba 2>/dev/null | wc -l)
LATE_AUDIO=$(grep -oE "音频块 [0-9]+" "$OUT/late.log" | grep -oE "[0-9]+" | head -1)
LATE_AUDIO=${LATE_AUDIO:-0}
echo "中途接入解码帧数: $LATE | 音频块: $LATE_AUDIO"

log "跨设备级联：中继 C（$RELAY_C）经 /api/proxy 拉 A 的流"
"$CLI" relay -p "$RELAY_C" > "$OUT/relayC.log" 2>&1 &
C_PID=$!
sleep 1.2
RESP=$(curl -s --max-time 2 -X POST http://127.0.0.1:$RELAY_C/api/proxy \
  -H 'Content-Type: application/json' \
  -d "{\"upstream\":\"ws://127.0.0.1:$PORT\",\"streamId\":\"$SID\"}")
echo "  POST /api/proxy → $RESP"
sleep 1
echo "  C 的 /api/streams → $(curl -s --max-time 2 http://127.0.0.1:$RELAY_C/api/streams | head -c 200)"
"$CLI" receive --relay "ws://127.0.0.1:$RELAY_C" --stream "$SID" --out "$OUT/cascade" --secs "$RECV_SECS" > "$OUT/cascade.log" 2>&1
CASCADE=$(ls "$OUT"/cascade/frame_*.rgba 2>/dev/null | wc -l)
CASCADE_AUDIO=$(grep -oE "音频块 [0-9]+" "$OUT/cascade.log" | grep -oE "[0-9]+" | head -1)
CASCADE_AUDIO=${CASCADE_AUDIO:-0}
echo "级联解码帧数: $CASCADE | 音频块: $CASCADE_AUDIO"

cleanup; trap - EXIT
echo
echo "直连=$DIRECT/$DIRECT_AUDIO 中途=$LATE/$LATE_AUDIO 级联=$CASCADE/$CASCADE_AUDIO（帧阈值 $MIN_FRAMES，音频阈值 $MIN_AUDIO）"
if [ "$DIRECT" -ge "$MIN_FRAMES" ] && [ "$LATE" -ge "$MIN_FRAMES" ] && [ "$CASCADE" -ge "$MIN_FRAMES" ] \
  && [ "$DIRECT_AUDIO" -ge "$MIN_AUDIO" ] && [ "$LATE_AUDIO" -ge "$MIN_AUDIO" ] && [ "$CASCADE_AUDIO" -ge "$MIN_AUDIO" ]; then
  echo "✅ 双设备端到端全部 OK"
else
  echo "❌ 存在失败项（直连/中途/级联任一帧数不足）"
  exit 1
fi
