#!/usr/bin/env bash
# Stross QUIC 硬断连（force-stop）流回收验证（回归脚本，阶段 C 断连检测）。
#
# 场景：设备 A = stross serve（受控中继，QUIC 33464）；独立推流进程经
# `stross push --relay quic://...` 推入 A；推流进程被 SIGKILL（等同手机
# force-stop，无 Bye/Close 帧）→ 断言 A 的 /api/streams 在「服务端 idle
# 超时（15s）+ 余量」内移除该流。
#
# 背景（真机实测暴露）：quinn 默认 idle 30s 且无 keepalive，force-stop 后
# 流残留约半分钟；现服务端 idle=15s + 客户端 keepalive=10s。
#
# 用法：scripts/quic-stale-stream-test.sh
# 退出码：0 = 流在预期窗口内被回收（并打印实测回收耗时）。
set -uo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
CLI="${CLI:-$REPO/target/debug/stross}"
OUT="${OUT:-/tmp/stross-quic-stale}"
PORT="${PORT:-18777}"
CTRL="${CTRL:-18778}"
QUIC="${QUIC:-33464}"
IDLE_GRACE=25   # 断言窗口（秒）：idle 15s + poll 间隔 + 余量

[ -x "$CLI" ] || cargo build -p stross-cli
rm -rf "$OUT" && mkdir -p "$OUT"
log() { printf '\n\033[1;34m== %s ==\033[0m\n' "$*"; }

cleanup() {
  kill "${A_PID:-}" 2>/dev/null || true
  kill "${PUSH_PID:-}" 2>/dev/null || true
}
trap cleanup EXIT

log "设备 A：serve（受控中继 + QUIC $QUIC）"
"$CLI" serve --port "$PORT" --ctrl-port "$CTRL" --quic-port "$QUIC" > "$OUT/serve.log" 2>&1 &
A_PID=$!
sleep 1.2

SID=$("$CLI" ctrl create-session --title "quic-stale" 2>&1 | grep -o 'sessionId: [a-z0-9-]*' | awk '{print $2}')
[ -n "$SID" ] || { echo "✗ 建会话失败"; tail -5 "$OUT/serve.log"; exit 1; }
log "会话: $SID —— 独立推流进程经 QUIC 推入（持久 300s，等被 SIGKILL）"

"$CLI" push --relay "quic://127.0.0.1:$QUIC" --stream-id "$SID" --secs 300 --audio > "$OUT/push.log" 2>&1 &
PUSH_PID=$!
sleep 3
if ! curl -s --max-time 2 "http://127.0.0.1:$PORT/api/streams" | grep -q "$SID"; then
  echo "✗ 推流未出现在 /api/streams（push 可能被拒）"
  tail -8 "$OUT/push.log"
  exit 1
fi
echo "✓ 流已建立: $SID（watchers=$(curl -s http://127.0.0.1:$PORT/api/streams | python3 -c 'import sys,json; d=json.load(sys.stdin); print(d[0]["watchers"] if d else 0)' 2>/dev/null)）"

log "SIGKILL 推流进程（等同手机 force-stop，无再见帧）"
kill -9 "$PUSH_PID"
PUSH_PID=""
echo "  推流进程已 kill -9"

log "轮询 /api/streams，断言 $IDLE_GRACE 秒内移除"
START=$(date +%s)
for i in $(seq 1 "$IDLE_GRACE"); do
  sleep 1
  if ! curl -s --max-time 2 "http://127.0.0.1:$PORT/api/streams" | grep -q "$SID"; then
    ELAPSED=$(( $(date +%s) - START ))
    echo "✓ 流 $SID 在 ${ELAPSED}s 后从 /api/streams 移除（idle=15s 预算内）"
    cleanup
    exit 0
  fi
done
echo "✗ ${IDLE_GRACE}s 内流未被回收（流残留）——idle 检测失效？"
echo "  残留 /api/streams: $(curl -s --max-time 2 http://127.0.0.1:$PORT/api/streams | head -c 300)"
exit 1