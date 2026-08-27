#!/usr/bin/env bash
# Stross 推流端静默（非优雅断开）流回收验证（回归脚本，阶段 C 断连检测）。
#
# 场景：设备 A = stross serve（受控中继，SRT 33462）；独立推流进程经
# `stross push --relay srt://...` 推入 A；推流进程被 SIGKILL（等同手机
# force-stop，无 Bye/Close 帧）→ 断言 A 的 /api/streams 在「数据面静默
# 看门狗（10s）+ 轮询余量」内移除该流。
#
# 背景（真机实测暴露）：rsrt 的 peer-idle 检测要求 EXP 计数超限 + 5s
# 无包，对 SIGKILL 的 UDP 对端可能永远不触发（实测 40 分钟未回收）；
# 现数据面在 handle_push_loop/proxy_uplink 加 10s 无消息看门狗，传输无关。
#
# 另验证观看端自愈：僵尸流被删除后，其广播 channel 关闭 → handle_watch
# 退出 → watchers 归零（防止观看数泄漏）。
#
# 用法：scripts/srt-push-silence-cleanup-test.sh
# 退出码：0 = 流在预期窗口内被回收（并打印实测回收耗时）。
set -uo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
CLI="${CLI:-$REPO/target/debug/stross}"
OUT="${OUT:-/tmp/stross-srt-silence}"
PORT=18787         # 独立端口，避免干扰常驻 serve
CTRL=18788
SRT=33472
IDLE_GRACE=15      # 断言窗口（秒）：看门狗 10s + 轮询间隔 + 余量

[ -x "$CLI" ] || cargo build -p stross-cli
rm -rf "$OUT" && mkdir -p "$OUT"
log() { printf '\n\033[1;34m== %s ==\033[0m\n' "$*"; }

cleanup() {
  kill "${A_PID:-}" 2>/dev/null || true
  kill "${PUSH_PID:-}" 2>/dev/null || true
  kill "${WATCH_PID:-}" 2>/dev/null || true
}
trap cleanup EXIT

log "设备 A：serve（受控中继 + SRT $SRT）在端口 $PORT"
"$CLI" serve --port "$PORT" --ctrl-port "$CTRL" --srt-port "$SRT" > "$OUT/serve.log" 2>&1 &
A_PID=$!
sleep 1.2

SID=$("$CLI" ctrl --connect "ws://127.0.0.1:$CTRL/ws/ctrl" create-session --title "srt-silence" 2>&1 | grep -o 'sessionId: [a-z0-9-]*' | awk '{print $2}')
[ -n "$SID" ] || { echo "✗ 建会话失败"; tail -5 "$OUT/serve.log"; exit 1; }
log "会话: $SID —— 独立推流进程经 SRT 推入（持久 300s，等被 SIGKILL）"

"$CLI" push --relay "srt://127.0.0.1:$SRT" --stream-id "$SID" --secs 300 --audio > "$OUT/push.log" 2>&1 &
PUSH_PID=$!
sleep 3
if ! curl -s --max-time 2 "http://127.0.0.1:$PORT/api/streams" | grep -q "$SID"; then
  echo "✗ 推流未出现在 /api/streams（push 可能被拒）"
  tail -8 "$OUT/push.log"
  exit 1
fi
echo "✓ 流已建立: $SID"

log "挂一个观看端（SRT watch）应能看到流"
"$CLI" receive --relay "srt://127.0.0.1:$SRT" --stream "$SID" --out "$OUT/watch" --secs 60 > "$OUT/watch.log" 2>&1 &
WATCH_PID=$!
sleep 2
W=$(curl -s --max-time 2 "http://127.0.0.1:$PORT/api/streams" | python3 -c 'import sys,json; d=json.load(sys.stdin); print(d[0]["watchers"] if d else 0)' 2>/dev/null)
echo "  观看端接入后 watchers=$W"
[ "$W" -ge 1 ] || { echo "✗ watchers 未增长（观看端未接入？）"; tail -5 "$OUT/watch.log"; exit 1; }

log "SIGKILL 推流进程（等同手机 force-stop，无再见帧）"
kill -9 "$PUSH_PID"
PUSH_PID=""
echo "  推流进程已 kill -9"

log "轮询 /api/streams，断言 $IDLE_GRACE 秒内移除（静默看门狗 10s）"
START=$(date +%s)
for i in $(seq 1 "$IDLE_GRACE"); do
  sleep 1
  if ! curl -s --max-time 2 "http://127.0.0.1:$PORT/api/streams" | grep -q "$SID"; then
    ELAPSED=$(( $(date +%s) - START ))
    echo "✓ 流 $SID 在 ${ELAPSED}s 后从 /api/streams 移除（看门狗预算内）"
    # 等 watch 端随广播 channel 关闭而退出，watchers 应归零
    sleep 2
    REM=$(curl -s --max-time 2 "http://127.0.0.1:$PORT/api/streams")
    echo "  移除后 /api/streams: ${REM:-空}"
    echo "✓ 静默看门狗回收验证通过"
    cleanup
    exit 0
  fi
done
echo "✗ ${IDLE_GRACE}s 内流未被回收（看门狗未生效？）"
echo "  残留 /api/streams: $(curl -s --max-time 2 http://127.0.0.1:$PORT/api/streams | head -c 300)"
tail -5 "$OUT/serve.log"
exit 1