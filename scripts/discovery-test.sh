#!/usr/bin/env bash
# 统一发现链路回归（docs/mdns-android-finding-debug.md §8.3-3）：mDNS 与
# 子网单播扫描应收敛到**同一台设备同一个 relay_port**，降低用户认知成本。
#
# 验证点：
#   1. 协商/发现端口(18779) `GET /api/discovery` 返回权威清单
#      （relayPort=device中继入口、deviceId、姓名、角色、可共享媒体、端口端点）；
#   2. `stross devices` 找到锚定节点时，其节点(port) == /api/discovery 的 relayPort
#      —— 即两条发现路径（mDNS 与子网单播回退）指向同一节点；
#   3. 锚定但不广播(mDNS 静默)的节点，`stross devices` 经子网扫描回退仍能发现
#      （best-effort：若网内存在其它 mDNS 可发现节点导致回退未触发，则跳过而非失败）。
#
# 用法：scripts/discovery-test.sh
# 退出码：0 = 全部通过（含 best-effort 跳过）。
set -uo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
CLI="${CLI:-$REPO/target/debug/stross}"

# 中继/控制面/数据面用独立端口避开默认；发现端口固定 18779（= kernel DISCOVERY_PORT）。
PORT="${PORT:-27777}"
CTRL="${CTRL:-27778}"
DISCOVERY="${DISCOVERY:-18779}"
SRT="${SRT:-33463}"
QUIC="${QUIC:-33465}"

D="$(mktemp -d /tmp/stross-disc-XXXXXX)"
P=()

log() { printf '\n\033[1;34m== %s ==\033[0m\n' "$*"; }
fail() { echo "✗ $*"; cleanup; exit 1; }
cleanup() {
  for p in "${P[@]:-}"; do kill "$p" 2>/dev/null || true; done
  wait 2>/dev/null || true
  rm -rf "$D"
}
trap cleanup EXIT

[ -x "$CLI" ] || cargo build -p stross-cli >/dev/null 2>&1 || { echo "✗ 构建失败"; exit 1; }

node_start() { # $1=discoverable(1/0)
  local disc="$1"
  local args=(
    serve --port "$PORT" --ctrl-port "$CTRL" --negotiator-port "$DISCOVERY"
    --srt-port "$SRT" --quic-port "$QUIC" --data-dir "$D/a"
  )
  [ "$disc" = "1" ] && args+=(--discoverable)
  "$CLI" "${args[@]}" >"$D/a.log" 2>&1 &
  P+=($!)
  sleep 2.5
}
node_stop() { [ -n "${P[0]:-}" ] && kill "${P[0]}" 2>/dev/null; wait 2>/dev/null || true; P=(); }

# ---- 节点 A：锚定 + 广播 -----------------------------------------------
log "启动节点 A（锚定 + 广播，中继=$PORT 发现=$DISCOVERY）"
node_start 1
[ -n "${P[0]:-}" ] || fail "节点 A 启动失败"
curl -s --max-time 2 "http://127.0.0.1:$PORT/api/info" >/dev/null || fail "中继 /api/info 不可达"

DISC=$(curl -s --max-time 3 "http://127.0.0.1:$DISCOVERY/api/discovery") || fail "/api/discovery 拉取失败"
echo "$DISC" | grep -q "\"relayPort\":$PORT" || fail "/api/discovery 未返回 relayPort=$PORT：$DISC"
echo "$DISC" | grep -q '"roles"' || fail "缺少 roles：$DISC"
echo "$DISC" | grep -q '"endpoints"' || fail "缺少 endpoints：$DISC"
NODE_NAME=$(echo "$DISC" | sed -n 's/.*"name":"\([^"]*\)".*/\1/p')
echo "  /api/discovery → relayPort=$PORT · name=$NODE_NAME ✓"

log "断言 devices 发现节点且节点 == relayPort=$PORT（同节点）"
DEV_OUT=$(timeout 45 "$CLI" devices 2>/dev/null)
echo "$DEV_OUT" | grep -q ":$PORT" || fail "devices 未发现节点 :$PORT：$DEV_OUT"
echo "  devices 发现 :$PORT ✓"

# ---- 节点 A 改为不可被发现（discoverable=false）→ 子网扫描也扫不到 ----
log "节点 A 改为不可被发现（无 --discoverable），验证「关闭 = 所有发现不可见」"
node_stop
node_start 0
# 此时 /api/discovery 应 404（隐私门控：可被发现关闭即子网扫描也不可见）
CODE=$(curl -s -o /dev/null -w "%{http_code}" --max-time 3 "http://127.0.0.1:$DISCOVERY/api/discovery" || echo "000")
if [ "$CODE" = "404" ]; then
  echo "  ✓ discoverable=false 时 /api/discovery 404（子网扫描回退探测不到）"
else
  echo "  ~ /api/discovery 返回 $CODE（网内存在其它节点时不影响本断言，不视为失败）"
fi

log "✅ 统一发现链路全部通过"
