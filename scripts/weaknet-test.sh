#!/usr/bin/env bash
# Stross 弱网稳定性测试（需求硬指标：弱网不崩）。
#
# 本机 sudo 需要密码，主机 lo 上不能直接 `tc qdisc`；改用 `unshare -rn`
# （用户 + 网络命名空间）在命名空间内的 lo 上注入 netem 规则，serve /
# push / receive 全程跑在同一命名空间内、按 127.0.0.1 走回环，数据面
# 流量因此真实经过 netem（丢包 / 延迟注入）。命名空间销毁即自动摘除规则。
#
# 场景矩阵（逗号分隔多个场景，每场景 = "丢包率 延迟"）：
#   5% 20ms   — 轻度弱网（无线偶发丢包 / 跨房间）
#   10% 40ms  — 中度弱网（弱 Wi-Fi / 拥塞）
# 传输（复用 latency-stability-test.sh 的 ws/srt/quic 断言）：
#   帧数 ≥ 90%、音频块 ≥ 90%、绝对延迟 p99−min ≤ 250ms。
#   注意：弱网下 SRT/QUIC 的丢帧与延迟上涨属诊断信息，脚本原样上报、
#   不以退出码掩盖，供后续调参（如 SrtOptions）决策。
#
# 用法：scripts/weaknet-test.sh [SECS] [SCENARIOS] [TRANS...]
#   例：scripts/weaknet-test.sh 60 "5% 20ms,10% 40ms" "ws srt quic"
# 退出码：0 = 所有场景全部达标；1 = 存在未达标（各传输行已逐条打印）。
set -uo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${OUT:-/tmp/stross-weaknet}"
SECS="${1:-60}"
SCENARIOS="${2:-5% 20ms,10% 40ms}"
TRANS="${3:-ws srt quic}"

rm -rf "$OUT" && mkdir -p "$OUT"
log() { printf '\n\033[1;34m== %s ==\033[0m\n' "$*"; }

OVERALL=0
IFS=',' read -ra scen_list <<< "$SCENARIOS"
for scen in "${scen_list[@]}"; do
  # shellcheck disable=SC2086
  set -- $scen          # "5% 20ms" → $1=丢包率 $2=延迟
  local_loss=$1
  local_delay=$2
  log "弱网场景: 丢包 ${local_loss} / 延迟 ${local_delay}（${SECS}s × ${TRANS}）"

  # 命名空间内：拉起 lo → 注入 netem → 校准实测丢包/RTT → 跑全套测试
  unshare -rn bash -c "
    ip link set lo up   # 新网络命名空间里 lo 默认 DOWN，必须先拉起
    tc qdisc add dev lo root netem loss '$local_loss' delay '$local_delay' 2>/dev/null \
      || { echo '  [ns] netem 注入失败'; exit 2; }
    echo '  [ns] netem 已注入 lo: loss=$local_loss delay=$local_delay'
    ping -c 10 -q 127.0.0.1 2>/dev/null | tail -1 | sed 's/^/  [ns] 回环实测: /'
    set +e
    '$REPO/scripts/latency-stability-test.sh' '$SECS' '$TRANS'
    exit \$?
  " 2>&1 | tee "$OUT/${local_loss}-${local_delay}.log"
  rc=${PIPESTATUS[0]}

  echo ""
  echo "  场景判定（loss=${local_loss} delay=${local_delay}, 内层 rc=${rc}）:"
  grep -E "✅|❌" "$OUT/${local_loss}-${local_delay}.log" | sed 's/^/    /'
  if [ "$rc" -eq 0 ]; then
    echo "  ✅ 场景全过"
  else
    echo "  ❌ 场景存在未达标（弱网诊断信息见上，供调参决策）"
    OVERALL=1
  fi
done

echo ""
if [ "$OVERALL" -eq 0 ]; then
  echo "✅ 弱网测试全部场景达标"
else
  echo "❌ 弱网测试存在未达标场景（弱网下为预期诊断，不掩盖）"
  exit 1
fi