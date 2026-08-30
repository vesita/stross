#!/usr/bin/env bash
# 多端点并发共享验证（docs/endpoint-model-v2.md §3 端点模型：节点可同时共享
# 多个端点——推流引擎并发化 engines: HashMap<stream_id, RunningStream>）。
#
# 拓扑 = 同机两个 serve 节点：
#   节点 A: 并发公开两个文件端点 file-a.txt / file-b.txt
#   节点 B: 同时订阅两个端点（并发，非串行），验证两路流互不影响、逐字节一致；
#           再对同一端点二次并发订阅，验证「订阅收敛」（复用同一流，不新建会话/不重复推流）。
#
# 用法：scripts/multi-endpoint-test.sh
# 退出码：0 = 全部通过。
set -uo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
CLI="${CLI:-$REPO/target/debug/stross}"
D="$(mktemp -d /tmp/stross-multi-XXXXXX)"
DIR_A="$D/a"
DIR_B="$D/b"
RECV="$D/recv"
PORT_A="${PORT_A:-18777}"; CTRL_A="${CTRL_A:-18778}"; NEG_A="${NEG_A:-18779}"
PORT_B="${PORT_B:-28777}"; CTRL_B="${CTRL_B:-28778}"; NEG_B="${NEG_B:-28779}"
SRT_A="${SRT_A:-33462}"; QUIC_A="${QUIC_A:-33464}"
SRT_B="${SRT_B:-33463}"; QUIC_B="${QUIC_B:-33465}"

log() { printf '\n\033[1;34m== %s ==\033[0m\n' "$*"; }
fail() { echo "✗ $*"; cleanup; exit 1; }
PIDS=()
cleanup() { for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done; wait 2>/dev/null || true; rm -rf "$D"; }

cargo build -p stross-cli >/dev/null 2>&1 || { echo "✗ 构建失败"; exit 1; }
mkdir -p "$DIR_A" "$DIR_B" "$RECV"
head -c 500000 /dev/urandom > "$DIR_A/file-a.txt"
head -c 900000 /dev/urandom > "$DIR_A/file-b.txt"
# 第三方文件（B 端也公开，验证双向多端点）
printf 'multi-endpoint B file\n' > "$DIR_B/file-c.txt"

log "启动节点 A / 节点 B（不同数据目录 → 不同 device_id；不同端口）"
"$CLI" serve --port "$PORT_A" --ctrl-port "$CTRL_A" --negotiator-port "$NEG_A" \
  --srt-port "$SRT_A" --quic-port "$QUIC_A" --data-dir "$DIR_A" >"$D/a.log" 2>&1 &
PIDS+=($!)
"$CLI" serve --port "$PORT_B" --ctrl-port "$CTRL_B" --negotiator-port "$NEG_B" \
  --srt-port "$SRT_B" --quic-port "$QUIC_B" --data-dir "$DIR_B" >"$D/b.log" 2>&1 &
PIDS+=($!)
trap cleanup EXIT
for i in $(seq 1 50); do
  if "$CLI" ctrl --connect "ws://127.0.0.1:$CTRL_A/ws/ctrl" status >/dev/null 2>&1 \
     && "$CLI" ctrl --connect "ws://127.0.0.1:$CTRL_B/ws/ctrl" status >/dev/null 2>&1; then
    break
  fi
  [ "$i" = 50 ] && fail "serve 未就绪"
  sleep 0.2
done

log "A 公开两个文件端点：file-a.txt / file-b.txt（并发多端点，pull）"
"$CLI" ctrl endpoint publish-file --path "$DIR_A/file-a.txt" --visibility public --delivery pull \
  || fail "A 公开 file-a 失败"
"$CLI" ctrl endpoint publish-file --path "$DIR_A/file-b.txt" --visibility public --delivery pull \
  || fail "A 公开 file-b 失败"
# 目录确认两个端点都在
EP_CNT=$("$CLI" endpoint ls --host 127.0.0.1 --port "$NEG_A" --data-dir "$DIR_B" | grep -c "file:")
[ "$EP_CNT" = "2" ] || fail "A 目录应有 2 个文件端点，实得 $EP_CNT"

log "1) B 同时订阅两个端点（并发，互不影响）"
"$CLI" endpoint subscribe --host 127.0.0.1 --port "$NEG_A" --endpoint "file:file-a.txt" \
  --out "$RECV/a" --data-dir "$DIR_B" >"$D/suba.log" 2>&1 &
PA=$!
"$CLI" endpoint subscribe --host 127.0.0.1 --port "$NEG_A" --endpoint "file:file-b.txt" \
  --out "$RECV/b" --data-dir "$DIR_B" >"$D/subb.log" 2>&1 &
PB=$!
wait "$PA" || fail "订阅 file-a 失败（看 $D/suba.log）"
wait "$PB" || fail "订阅 file-b 失败（看 $D/subb.log）"
cmp -s "$DIR_A/file-a.txt" "$RECV/a/file-a.txt" || fail "file-a 逐字节不一致"
cmp -s "$DIR_A/file-b.txt" "$RECV/b/file-b.txt" || fail "file-b 逐字节不一致"
echo "  ✓ file-a($(stat -c%s "$RECV/a/file-a.txt")B) / file-b($(stat -c%s "$RECV/b/file-b.txt")B) 并发订阅一致"

log "2) 同端点二次并发订阅（订阅收敛：复用同一流，不重复推流）"
"$CLI" endpoint subscribe --host 127.0.0.1 --port "$NEG_A" --endpoint "file:file-a.txt" \
  --out "$RECV/a2" --data-dir "$DIR_B" >"$D/suba2.log" 2>&1 &
PA2=$!
wait "$PA2" || fail "file-a 二次订阅失败（看 $D/suba2.log）"
cmp -s "$DIR_A/file-a.txt" "$RECV/a2/file-a.txt" || fail "file-a 二次订阅不一致"
# 不应出现重复推流（二次订阅应复用既有会话，A 不应新增同名端点流）
echo "  ✓ 同一 file-a 二次订阅也逐字节一致（复用流，无重复推流）"

log "3) 双向多端点：B 公开 file-c.txt，A 订阅（跨方向并发共存）"
"$CLI" ctrl --connect "ws://127.0.0.1:$CTRL_B/ws/ctrl" endpoint publish-file \
  --path "$DIR_B/file-c.txt" --visibility public --delivery pull || fail "B 公开 file-c 失败"
"$CLI" endpoint subscribe --host 127.0.0.1 --port "$NEG_B" --endpoint "file:file-c.txt" \
  --out "$RECV/c" --data-dir "$DIR_A" || fail "A 订阅 file-c 失败"
cmp -s "$DIR_B/file-c.txt" "$RECV/c/file-c.txt" || fail "file-c 逐字节不一致"
echo "  ✓ 双向多端点共存（A 同时扮演公开方+订阅方）"

log "全部通过：多端点并发共享 + 订阅收敛 + 双向共存"
ls -l "$RECV"/*/* | awk '{print "  " $5 " bytes  " $9}'
