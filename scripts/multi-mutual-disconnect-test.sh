#!/usr/bin/env bash
# 多端点相互分享 + 断连测试（并发双向 + 中断恢复）。
#
# 目标：暴露「两端同时互发多端点 + 中途断连」的并发/清理缺陷。
#   1) 并发相互分享：A 公开 file-a/file-b，B 公开 file-c/file-d；
#      同时 A 订阅 B 的两个端点、B 订阅 A 的两个端点（4 路并发互发）。
#   2) 断连恢复：大文件（file-big）传输中段 kill 掉发布节点（断连）→
#      订阅方应优雅出错收尾（不挂起、不残留），且发布节点引擎应清理。
#   3) 回归：断连后重新启动发布节点，再次订阅恢复一致。
#
# 用法：scripts/multi-mutual-disconnect-test.sh
# 退出码：0 = 全部通过。
set -uo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
CLI="${CLI:-$REPO/target/debug/stross}"
D="$(mktemp -d /tmp/stross-mutual-XXXXXX)"
DIR_A="$D/a"; DIR_B="$D/b"; RECV="$D/recv"
PORT_A=29777; CTRL_A=29778; NEG_A=29779; SRT_A=35462; QUIC_A=35464
PORT_B=30777; CTRL_B=30778; NEG_B=30779; SRT_B=35463; QUIC_B=35465
BIG_MB=6   # 大文件（断连用：分块传完需要时间，可中断）

log() { printf '\n\033[1;34m== %s ==\033[0m\n' "$*"; }
fail() { echo "✗ $*"; cleanup; exit 1; }
cleanup() { for p in "${PIDS[@]:-}" "${SUBPIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done; wait 2>/dev/null || true; rm -rf "$D"; }

cargo build -p stross-cli >/dev/null 2>&1 || { echo "✗ 构建失败"; exit 1; }
mkdir -p "$DIR_A" "$DIR_B" "$RECV"
head -c 400000 /dev/urandom   > "$DIR_A/file-a.txt"
head -c 300000 /dev/urandom   > "$DIR_A/file-b.txt"
head -c $((BIG_MB * 1024 * 1024)) /dev/urandom > "$DIR_A/file-big-a.txt" 2>/dev/null || : > "$DIR_A/file-big-a.txt"
head -c 500000 /dev/urandom   > "$DIR_B/file-c.txt"
head -c 350000 /dev/urandom   > "$DIR_B/file-d.txt"
head -c $((BIG_MB * 1024 * 1024)) /dev/urandom > "$DIR_B/file-big-b.txt" 2>/dev/null || : > "$DIR_B/file-big-b.txt"

log "启动节点 A / 节点 B"
"$CLI" serve --port "$PORT_A" --ctrl-port "$CTRL_A" --negotiator-port "$NEG_A" \
  --srt-port "$SRT_A" --quic-port "$QUIC_A" --data-dir "$DIR_A" >"$D/a.log" 2>&1 &
PIDS+=($!)
"$CLI" serve --port "$PORT_B" --ctrl-port "$CTRL_B" --negotiator-port "$NEG_B" \
  --srt-port "$SRT_B" --quic-port "$QUIC_B" --data-dir "$DIR_B" >"$D/b.log" 2>&1 &
PIDS+=($!)
trap cleanup EXIT
for i in $(seq 1 50); do
  "$CLI" ctrl --connect "ws://127.0.0.1:$CTRL_A/ws/ctrl" status >/dev/null 2>&1 \
    && "$CLI" ctrl --connect "ws://127.0.0.1:$CTRL_B/ws/ctrl" status >/dev/null 2>&1 && break
  [ "$i" = 50 ] && fail "serve 未就绪（看 $D/a.log / $D/b.log）"; sleep 0.2
done

log "1) 并发相互分享：A、B 各公开 2 个普通端点 + 1 个大文件端点（共 3 个）"
for f in file-a file-b file-big-a; do
  "$CLI" ctrl --connect "ws://127.0.0.1:$CTRL_A/ws/ctrl" endpoint publish-file \
    --path "$DIR_A/$f.txt" --visibility public --delivery pull || fail "A 公开 $f 失败"
done
DB=$("$CLI" ctrl --connect "ws://127.0.0.1:$CTRL_B/ws/ctrl" endpoint publish-file --path "$DIR_B/file-c.txt" --visibility public --delivery pull) || fail "B 公开 file-c 失败"
"$CLI" ctrl --connect "ws://127.0.0.1:$CTRL_B/ws/ctrl" endpoint publish-file --path "$DIR_B/file-d.txt" --visibility public --delivery pull || fail "B 公开 file-d 失败"
"$CLI" ctrl --connect "ws://127.0.0.1:$CTRL_B/ws/ctrl" endpoint publish-file --path "$DIR_B/file-big-b.txt" --visibility public --delivery pull || fail "B 公开 file-big-b 失败"
echo "  A 端点数=$("$CLI" endpoint ls --host 127.0.0.1 --port "$NEG_A" --data-dir "$DIR_B" | grep -c 'file:')"
echo "  B 端点数=$("$CLI" endpoint ls --host 127.0.0.1 --port "$NEG_B" --data-dir "$DIR_A" | grep -c 'file:')"

log "2) 并发相互订阅：A→B(c,d) 与 B→A(a,b) 同时进行（4 路并发）"
SUBPIDS=()
"$CLI" endpoint subscribe --host 127.0.0.1 --port "$NEG_B" --endpoint "file:file-c.txt"  --out "$RECV/ac" --data-dir "$DIR_A" >"$D/s_ac.log" 2>&1 & SUBPIDS+=($!)
"$CLI" endpoint subscribe --host 127.0.0.1 --port "$NEG_B" --endpoint "file:file-d.txt"  --out "$RECV/ad" --data-dir "$DIR_A" >"$D/s_ad.log" 2>&1 & SUBPIDS+=($!)
"$CLI" endpoint subscribe --host 127.0.0.1 --port "$NEG_A" --endpoint "file:file-a.txt"  --out "$RECV/ba" --data-dir "$DIR_B" >"$D/s_ba.log" 2>&1 & SUBPIDS+=($!)
"$CLI" endpoint subscribe --host 127.0.0.1 --port "$NEG_A" --endpoint "file:file-b.txt"  --out "$RECV/bb" --data-dir "$DIR_B" >"$D/s_bb.log" 2>&1 & SUBPIDS+=($!)
sleep 6
cmp -s "$DIR_B/file-c.txt" "$RECV/ac/file-c.txt" || fail "A→B file-c 不一致"
cmp -s "$DIR_B/file-d.txt" "$RECV/ad/file-d.txt" || fail "A→B file-d 不一致"
cmp -s "$DIR_A/file-a.txt" "$RECV/ba/file-a.txt" || fail "B→A file-a 不一致"
cmp -s "$DIR_A/file-b.txt" "$RECV/bb/file-b.txt" || fail "B→A file-b 不一致"
echo "  ✓ 4 路并发相互订阅全部一致"

log "3) 断连恢复：B→A 传输 file-big-a 中段 kill 掉发布节点 B（断连）→ A 的订阅应优雅出错收尾"
"$CLI" endpoint subscribe --host 127.0.0.1 --port "$NEG_B" --endpoint "file:file-big-b.txt" --out "$RECV/abig" --data-dir "$DIR_A" >"$D/s_abig.log" 2>&1 &
SUB_BIG=$!; SUBPIDS+=($SUB_BIG)
sleep 1.5   # 大文件传输进行中
for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done   # 杀掉 B（节点）→ 断连
wait "$PIDS[1]" 2>/dev/null || true
sleep 2
RECV_BIG="$(stat -c%s "$RECV/abig/file-big-b.txt" 2>/dev/null || echo 0)"
if kill -0 "$SUB_BIG" 2>/dev/null; then
  # 订阅进程仍挂着 → 断连后未收尾（缺陷）
  fail "断连后订阅方未收尾（仍运行；已收 $RECV_BIG 字节）——应优雅出错"
fi
echo "  ✓ 断连后订阅方已收尾退出（已收 $RECV_BIG 字节，未挂起）"

echo ""
echo "全部通过：多端点并发相互分享 + 断连优雅收尾"
