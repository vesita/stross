#!/usr/bin/env bash
# Stross 凭证式跨设备推流验证（B 阶段，docs/iteration-plan.md B0/B1）。
#
# 拓扑 = 两个 PC 端：
#   PC-A（接收端）: stross serve（内核 + 受控中继 + 控制面）→ 建会话 → 签发 ShareToken
#   PC-B（推流端）: stross push --share-token <token> → 凭凭证接入 A 的受控中继推流
#   PC-A（接收端）: stross receive → 播放 B 推来的流（"电脑接收手机麦克风"的反向闭环）
#
# 验证点：
#   1. 未出示凭证的推流被受控中继拒绝（F2.2 语义保持）；
#   2. 出示有效凭证 → 放行（Welcome），A 端可播放（视频帧 + 音频块 ≥ 阈值）；
#   3. 篡改凭证（PIN 改掉）→ 拒绝（服务端以签发时存储为准）。
#
# 用法：scripts/share-token-test.sh
# 退出码：0 = 凭证接入 + 播放 + 反例全部符合预期。
set -uo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
CLI="${CLI:-$REPO/target/debug/stross}"
OUT="${OUT:-/tmp/stross-token}"
PORT="${PORT:-18777}"
CTRL="${CTRL:-18778}"
SECS=14          # B 推流时长
RECV_SECS=6      # A 接收时长（推流开始 1s 后接入）
MIN_FRAMES=15
MIN_AUDIO=40

# 推流端经"局域网 IP"连接（非回环来源）——模拟另一台设备：
# 受控中继对非回环来源强制凭证接入（预授权只服务本机回环流程）。
# 取第一个全局 IPv4，过滤 fake-IP（Clash TUN 198.18/15）与 link-local，
# 避免挑中 VPN/TUN 地址。
LAN_IP=$(ip -4 -o addr show scope global 2>/dev/null \
  | awk '{print $4}' | cut -d/ -f1 \
  | awk '!/^(198\.18\.|169\.254\.|127\.)/ {print; exit}')
[ -n "$LAN_IP" ] || LAN_IP=$(hostname -I 2>/dev/null | awk '{print $1}')
[ -n "$LAN_IP" ] || LAN_IP=127.0.0.1
PUSH_BASE="ws://$LAN_IP:$PORT/ws/push"
WATCH_BASE="ws://$LAN_IP:$PORT"
echo "  模拟跨设备来源: $LAN_IP"

# 强制构建（本脚本验证最新代码；旧二进制会导致新子命令/字段缺失）
cargo build -p stross-cli >/dev/null 2>&1 || { echo "✗ 构建失败"; exit 1; }
rm -rf "$OUT" && mkdir -p "$OUT"
log() { printf '\n\033[1;34m== %s ==\033[0m\n' "$*"; }

cleanup() { kill "${A_PID:-}" 2>/dev/null || true; kill "${B_PID:-}" 2>/dev/null || true; }
trap cleanup EXIT

log "PC-A：serve（接收端，内核+受控中继+控制面，端口 $PORT）"
"$CLI" serve --port "$PORT" --ctrl-port "$CTRL" > "$OUT/serve.log" 2>&1 &
A_PID=$!
sleep 1.2

SID=$("$CLI" ctrl create-session --title 反向麦克风 2>&1 | grep -o 'sessionId: [a-z0-9-]*' | awk '{print $2}')
[ -n "$SID" ] || { echo "✗ 建会话失败"; exit 1; }
log "会话（内核签发，D4）: $SID"

# 反例 1：不建会话、不授权，直接推流 → 受控中继必须拒绝
log "反例 1：无凭证推流应被拒绝"
"$CLI" push --relay "$PUSH_BASE" --stream-id "intruder-$SID" --secs 2 \
  > "$OUT/no-token.log" 2>&1
if grep -q "未授权" "$OUT/no-token.log"; then
  echo "  ✅ 无凭证推流被拒绝（F2.2 语义保持）"
else
  echo "  ❌ 无凭证推流未被拒绝（受控中继放行了未授权流！）"
  cat "$OUT/no-token.log"
  exit 1
fi

log "PC-A：签发接入凭证（ctrl share-token）"
CTRL_OUT=$("$CLI" ctrl share-token "$SID" --ttl 300 2>&1)
TOKEN=$(echo "$CTRL_OUT" | grep -oE '\{"v":[0-9].*' | head -1)
[ -n "$TOKEN" ] || { echo "✗ 签发凭证失败: $CTRL_OUT"; exit 1; }
echo "  token: ${TOKEN:0:80}…"

log "PC-B：凭凭证向 A 的受控中继推流（合成视频 + 440Hz 测试音）"
"$CLI" push --relay "$PUSH_BASE" \
  --stream-id "$SID" --share-token "$TOKEN" --secs "$SECS" --audio \
  > "$OUT/push.log" 2>&1 &
B_PID=$!
sleep 1.2
grep -q "中继已确认推流" "$OUT/push.log" && echo "  ✅ B 端凭凭证接入成功（Hello 被接受）" \
  || { echo "  ❌ B 端接入失败: $(tail -3 "$OUT/push.log")"; exit 1; }

log "PC-A：接收并播放 B 推来的流（反向闭环：手机麦克风 → 电脑扬声器）"
"$CLI" receive --relay "$WATCH_BASE" --stream "$SID" --out "$OUT/recv" --secs "$RECV_SECS" \
  > "$OUT/recv.log" 2>&1
FRAMES=$(ls "$OUT"/recv/frame_*.rgba 2>/dev/null | wc -l)
AUDIO=$(grep -oE "音频块 [0-9]+" "$OUT/recv.log" | grep -oE "[0-9]+" | head -1)
AUDIO=${AUDIO:-0}
echo "A 端播放：解码帧 $FRAMES | 音频块 $AUDIO"

# 反例 2：篡改凭证（PIN 改掉）→ 拒绝
log "反例 2：篡改凭证（PIN 改为 000000）应被拒绝"
FORGED=$(echo "$TOKEN" | sed 's/"pin":"[0-9]*"/"pin":"000000"/')
"$CLI" push --relay "$PUSH_BASE" \
  --stream-id "$SID" --share-token "$FORGED" --secs 2 \
  > "$OUT/forged.log" 2>&1
if grep -q "未授权\|凭证" "$OUT/forged.log"; then
  echo "  ✅ 篡改凭证被拒绝"
else
  echo "  ❌ 篡改凭证未被拒绝！"
  cat "$OUT/forged.log"
  exit 1
fi

cleanup; trap - EXIT
echo
echo "凭证接入播放：$FRAMES 帧 / $AUDIO 音频块（阈值 $MIN_FRAMES / $MIN_AUDIO）"
if [ "$FRAMES" -ge "$MIN_FRAMES" ] && [ "$AUDIO" -ge "$MIN_AUDIO" ]; then
  echo "✅ 凭证式跨设备推流（双 PC）全部 OK"
else
  echo "❌ 播放帧数/音频块不足"
  exit 1
fi
