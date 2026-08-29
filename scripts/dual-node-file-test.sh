#!/usr/bin/env bash
# 端点框架本地双端节点互发小文件验证（docs/endpoint-model.md §5/§3.6）。
#
# 拓扑 = 同机两个 serve 节点（不同数据目录 → 不同 device_id；不同端口）：
#   节点 A: stross serve --port 18777 --ctrl-port 18778 --negotiator-port 18779
#           --srt-port 33462 --quic-port 33464 --data-dir /tmp/stross-a
#   节点 B: stross serve --port 28777 --ctrl-port 28778 --negotiator-port 28779
#           --srt-port 33463 --quic-port 33465 --data-dir /tmp/stross-b
#
# 验证三条订阅链路（订阅方进程使用目标节点身份的数据目录，模拟"该节点订阅"）：
#   1. A→B pull ：B 订阅 A 的 file-a.txt（pull，连 A 中继 watch）
#   2. A→B push ：B 订阅 A 的 file-c.bin（push，A 凭 B 自签凭证出站推入 B 中继）
#   3. B→A pull ：A 订阅 B 的 file-b.txt（pull）
# 每条链路校验：订阅握手（Public 自动签发）→ 文件落盘 → cmp 逐字节一致。
#
# 用法：scripts/dual-node-file-test.sh
# 退出码：0 = 全部通过。
set -uo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
CLI="${CLI:-$REPO/target/debug/stross}"
D="$(mktemp -d /tmp/stross-dual-XXXXXX)"
DIR_A="$D/a"   # 节点 A 数据目录 + 文件
DIR_B="$D/b"   # 节点 B 数据目录 + 文件
RECV="$D/recv"
PORT_A="${PORT_A:-18777}"; CTRL_A="${CTRL_A:-18778}"; NEG_A="${NEG_A:-18779}"
PORT_B="${PORT_B:-28777}"; CTRL_B="${CTRL_B:-28778}"; NEG_B="${NEG_B:-28779}"
SRT_A="${SRT_A:-33462}"; QUIC_A="${QUIC_A:-33464}"
SRT_B="${SRT_B:-33463}"; QUIC_B="${QUIC_B:-33465}"
SIZE_A=800     # file-a.txt 大小（KiB）
SIZE_C=3       # file-c.bin 大小（小文件，覆盖"非整块"末帧路径）

log() { printf '\n\033[1;34m== %s ==\033[0m\n' "$*"; }
fail() { echo "✗ $*"; cleanup; exit 1; }
PIDS=()
cleanup() {
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
  wait 2>/dev/null || true
  rm -rf "$D"
}

cargo build -p stross-cli >/dev/null 2>&1 || { echo "✗ 构建失败"; exit 1; }
mkdir -p "$DIR_A" "$DIR_B" "$RECV"
# 确定性内容（可复现 + cmp 可比对）
head -c $((SIZE_A * 1024)) /dev/urandom > "$DIR_A/file-a.txt" 2>/dev/null \
  || { i=0; : > "$DIR_A/file-a.txt"; while [ $i -lt $SIZE_A ]; do printf 'A%064d\n' "$i" >> "$DIR_A/file-a.txt"; i=$((i+1)); done; }
printf 'hello push, 小文件末帧验证 ✅\n' > "$DIR_A/file-c.bin"
printf 'B 节点的小文件: %s\n' "$(date +%s)" > "$DIR_B/file-b.txt"
printf '世界，你好。跨节点文件互发。\n' >> "$DIR_B/file-b.txt"

log "启动节点 A（默认端口，数据目录 $DIR_A）"
"$CLI" serve --port "$PORT_A" --ctrl-port "$CTRL_A" --negotiator-port "$NEG_A" \
  --srt-port "$SRT_A" --quic-port "$QUIC_A" --data-dir "$DIR_A" >"$D/a.log" 2>&1 &
PIDS+=($!)
log "启动节点 B（自定义端口 2877x，数据目录 $DIR_B）"
"$CLI" serve --port "$PORT_B" --ctrl-port "$CTRL_B" --negotiator-port "$NEG_B" \
  --srt-port "$SRT_B" --quic-port "$QUIC_B" --data-dir "$DIR_B" >"$D/b.log" 2>&1 &
PIDS+=($!)
trap cleanup EXIT

# 等控制面就绪（协商端口随 serve 同时启动）
for i in $(seq 1 50); do
  if "$CLI" ctrl --connect "ws://127.0.0.1:$CTRL_A/ws/ctrl" status >/dev/null 2>&1 \
     && "$CLI" ctrl --connect "ws://127.0.0.1:$CTRL_B/ws/ctrl" status >/dev/null 2>&1; then
    break
  fi
  [ "$i" = 50 ] && fail "serve 未就绪（看 $D/a.log / $D/b.log）"
  sleep 0.2
done

log "A 公开文件端点：file-a.txt（pull）与 file-c.bin（push）"
"$CLI" ctrl endpoint publish-file --path "$DIR_A/file-a.txt" --visibility public --delivery pull \
  || fail "A 公开 file-a.txt 失败"
"$CLI" ctrl endpoint publish-file --path "$DIR_A/file-c.bin" --visibility public --delivery push \
  || fail "A 公开 file-c.bin 失败"
log "B 公开文件端点：file-b.txt（pull）"
"$CLI" ctrl --connect "ws://127.0.0.1:$CTRL_B/ws/ctrl" endpoint publish-file \
  --path "$DIR_B/file-b.txt" --visibility public --delivery pull || fail "B 公开 file-b.txt 失败"

log "1) A→B pull：B 订阅 file-a.txt"
"$CLI" endpoint subscribe --host 127.0.0.1 --port "$NEG_A" \
  --endpoint "file:file-a.txt" --out "$RECV/1" --data-dir "$DIR_B" || fail "订阅 pull 失败"
cmp -s "$DIR_A/file-a.txt" "$RECV/1/file-a.txt" || fail "1) pull 文件不一致"

log "2) A→B push：B 订阅 file-c.bin（交付方向 push，A 出站推入 B 中继）"
"$CLI" endpoint subscribe --host 127.0.0.1 --port "$NEG_A" \
  --endpoint "file:file-c.bin" --delivery push --out "$RECV/2" --data-dir "$DIR_B" \
  || fail "订阅 push 失败"
cmp -s "$DIR_A/file-c.bin" "$RECV/2/file-c.bin" || fail "2) push 文件不一致"

log "3) B→A pull：A 订阅 file-b.txt"
"$CLI" endpoint subscribe --host 127.0.0.1 --port "$NEG_B" \
  --endpoint "file:file-b.txt" --out "$RECV/3" --data-dir "$DIR_A" || fail "订阅 B 失败"
cmp -s "$DIR_B/file-b.txt" "$RECV/3/file-b.txt" || fail "3) B 文件不一致"

log "全部通过：3 条订阅链路（pull/push/pull）文件逐字节一致"
ls -l "$RECV"/*/* | awk '{print "  " $5 " bytes  " $9}'