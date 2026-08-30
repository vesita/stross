# 排查记录：Stross 真机 mDNS 发现「只回 PTR、对端扫不到」问题

> 文档用途：上下文压缩现场用，后续可直接续查。所有结论已附**日志/抓包证据**。
> 关联：本轮目标 = 真机（OPPO PLC110, serial 3B6F5ME8GCL4660T, Android 16, WiFi 192.168.11.60）
> PC = 192.168.11.61（enp6s0）。同步参考 `docs/iteration-plan.md`。

---

## 0. 结论（已排查完毕，根因不在代码）

- **Stross 的 mDNS 代码完全正常**：在**能正常投递多播的网络**（手机↔PC 经 USB 共享，同网段 10.159.157.0/24）上，
  **双向发现均成功**：
  - PC → 手机：`stross devices` 完整发现手机（`10.159.157.104:8777`，含 SRV+TXT+A、端点/SRT/QUIC、在线共享 0 条）。
  - 手机 → PC：手机设备列表出现 PC（`pico · 10.159.157.158:18777 · 中继/共享/接收`）。
- **WiFi（`Su` 路由）下失败的真因**：**该路由器不向手机转发「下行」多播**（IGMP snooping / 客户端隔离 / 访客网行为），
  而手机→PC 的「上行」多播正常。Net result：手机只收得到"自己 socket 的回环多播"+"单播"，收不到"从 AP 下来的多播"。
- **已排除的层**（均为"疑似"→已用日志/抓包钉死）：TUN、网络加速开关、`probing_count` 闸门、地址匹配失败、
  `handle_read` 接口家族检查丢弃、socket/MulticastLock 配置（MulticastLock 已持有）。

> 本文件保留为排查记录（含证据 + 现有工作树改动）。**环境根因与解决办法见文末 §8。**

---

## 1. 已确认事实（每条有证据）

### 1.1 现象：手机「能发现电脑、电脑发现不了手机」
- `stross devices`（PC 端浏览）：只扫到 PC 自己（.61），**扫不到手机（.60）**。
- 手机 UI 显示"已锚定 · mDNS 广播中 · 未发现局域网内其它设备"。
- 手机 IP 192.168.11.60（wlan0），PC 192.168.11.61（enp6s0），同 /24 网段。

### 1.2 网络抓包（Python 原始 socket，正确解码）结论
- **PC（.61）** 对 `_stross._tcp.local` 的 `PTR/ANY/SRV` 查询 → 回 **`QR=1` 完整 `A+PTR+SRV+TXT`**。
- **手机（.60）** 对同样查询 → **只回 `QR=0` + `PTR`（仅 1 条答案）**，从不回 `QR=1` 的 SRV/TXT/A。
- 手机周期性只发 `QR=0` 且 `qtype=PTR(12)`（**不是**探测包的 `ANY=255`）→ 说明它自己的 `prepare_announce` 在早期（`probing_count>0`）阶段从未发出 `QR=1`；`QR=0 PTR` 是它**自己 browse** 发的查询。
- 关键判别：手机`qtype`统计 = `QR=0, qtypes=(12,), answers=(1,0,0)`，**无 ANY=255 探测包**。

### 1.3 `set_requires_probe(false)` 修复 **已生效**（日志）但仍不够
- 手机端日志：`DIAG set_requires_probe(false) applied for instance=stross-6b576449-8777`，`mDNS 广播已开启`。
- `prepare_announce` 对 `wlan0`：`intf=wlan0 is_ipv4=true answers_count=4 require_probe=false` —— **完整公告构造成功**（PTR+SRV+TXT+A 四条）。
- 其它接口 `ifb1/dummy0/ifb0/lo/ccmni1/ccmni0` 返回 `None`（地址集为空，属正常）。
- 但**网络仍抓不到手机 QR=1**。

### 1.4 手机上 `if_addrs` 枚举（诊断 `my_ip_interfaces_inner` 打印 raw）
```
vgate0 fe80::8459:853d:bf82:34c6  up=true p2p=true lo=false
wlan0  fe80::7840:24ff:feb4:8c9c  up=true p2p=false lo=false
ifb2   fe80::4013:c9ff:fe25:49b7  up=true p2p=false lo=false
ccmni1 fe80::18cf:1d53:ee08:3f18  up=true p2p=false lo=false
ccmni1 240e:579:4c20:14ba:18cf:1d53:ee08:3f18  up=true p2p=false lo=false
ccmni0 fe80::18cf:1d53:e12d:e064  up=true p2p=false lo=false
ccmni0 240e:579:4c40:2e13:18cf:1d53:e12d:e064  up=true p2p=false lo=false
ifb1   fe80::8d:5aff:fe1f:f476  up=true p2p=false lo=false
ifb0   fe80::688b:3cff:fe4c:f81  up=true p2p=false lo=false
dummy0 fe80::dcdd:b5ff:fed0:beeb  up=true p2p=false lo=false
lo     ::1  up=true p2p=false lo=true
vgate0 172.30.242.158  up=true p2p=true lo=false
wlan0  192.168.11.60  up=true p2p=false lo=false
lo     127.0.0.1  up=true p2p=true lo=true   <- 注：lo 的 127.0.0.1 标注 lo=true（loopback）
```
- **关键：`my_intfs` 过滤 `is_p2p()` 后，wlan0/ifb0/ifb1/ifb2/dummy0/ccmni0/ccmni1 都 `up && !p2p && !lo` → 全进 `my_intfs`**。手机上有大量**虚拟接口**。

### 1.5 `Discovery::start` 传入地址
```
instance=stross-6b576449-8777 host=<hostname>.local.
addrs=[240e:579:4c20:14ba:18cf:1d53:ee08:3f18, 240e:579:4c40:2e13:18cf:1d53:e12d:e064, 172.30.242.158, 192.168.11.60]
```
- 含 wlan0 的 192.168.11.60 ✅（地址本身没错）。

---

## 2. 根因推断（当前最可信）

**分两层，均已排除前两层：**

1. ~~地址匹配失败（`get_addrs_on_my_intf_v4` 返回空）~~ —— **已排除**：诊断证明 `wlan0` 的 `intf_addrs=["192.168.11.60"]` 非空，`prepare_announce` 能构造 `answers_count=4`。
2. ~~探测闸门（`probing_count>0`）~~ —— **已绕过**：`set_requires_probe(false)` 生效，`prepare_announce` 不再 reject。
3. **仍存在**：手机上 `announce_service_on_intf` 对 `wlan0` 返回 `Some(out)`（answers_count=4），但**该 `QR=1` 包打不出网**（网络抓不到）。疑似 **`send_dns_outgoing` 在 Android 多网卡下发送接口选择错误**：
   - 大量虚拟接口（ifb0/ifb1/ifb2/dummy0/ccmni0/ccmni1）进 `my_intfs`，`send_dns_outgoing`/`multicast_on_intf` 按 `my_intf` 设置 `set_multicast_if_v4`/`set_multicast_if_v6` 与 socket 组播绑定。
   - 可能真实 `wlan0` 的 socket 未正确加入 `224.0.0.251` 组 / 发送接口被虚拟接口抢走，导致 wlan0 上的 `SRV/TXT/A` 应答/公告发不到组播。
   - PC 只有 `enp6s0` → 发送正常。

---

## 3. 已做改动（工作树）

### 已提交前（本 fork 新增，未提交）——见 `git status`
- `crates/stross-kernel/src/discovery.rs`：
  - `Discovery` 结构体扩展 `instance/host/addrs/port`；新增 `Discovery::redefine`（同 fullname 覆盖重注册）。
  - `Discovery::start`/`redefine` 调 `info.set_requires_probe(false)`（含注释说明）。
- `crates/mdns/src/service_daemon.rs`：
  - `ingest_records`（query-with-answer 的 answers 入库 + 触发 ServiceFound/parse）——本轮早期，已存在。
  - `handle_read` 接口 fallback（pktinfo.if_index 不在 my_intfs 时回退可用 IPv4 接口）。
  - **遗留**：当前 `service_daemon.rs` 里仍有 3 处 DIAG 日志（`DIAG set_requires_probe`、`DIAG announce_service_on_intf`、`DIAG handle_query SRV/TXT skipped`）——**排查完需清除**。
- `crates/stross-kernel/src/settings.rs`（未跟踪）：`discoverable` 设置持久化。
- `crates/stross-kernel/src/lib.rs`：re-export `Settings/load_settings/save_settings`。
- `crates/stross-kernel/src/kernel/mod.rs`：`discoverable: AtomicBool`、`apply_discoverable`、`try_register_mdns`、`mdns_info`、`set_discoverable`。
- `apps/stross-cli/src/serve.rs`：`--discoverable` 标志。
- `apps/stross-gui/src-tauri/{commands.rs,lib.rs}`：`discoverable_status`/`set_discoverable` 命令。
- `apps/stross-gui/web/*`：前端"可被发现"开关（.ts/.js/.html/.css）。
- `docs/iteration-plan.md`：迭代记录。

> ⚠️ 注意：`service_daemon.rs` 里诊断日志的清除工作尚未完成（见 §4.2）。

---

## 4. 下一步计划

### 4.1 优先：定位手机发送接口问题（用户已选「继续定位发送接口问题」）
- 读完 `send_dns_outgoing_impl`（约 service_daemon.rs:4877-4975）与 `multicast_on_intf`（4990-5020），确认：
  - 手机上有哪些 `my_intf` 参与发送；`set_multicast_if_v4/v6` 实际用哪个接口。
  - **为什么 `wlan0` 的 `QR=1` 公告/应答打不出网**（socket 组播组绑定错、发送接口被虚拟接口覆盖、或发送目标地址错）。
- 候选修复：**限制 mdns 只用真实 LAN 接口（wlan0）做发送**，排除 ifb0/ifb1/ifb2/dummy0/ccmni0/ccmni1 等虚拟接口（`mdns` 的 `my_ip_interfaces_inner` 需过滤掉这些，或发送时按「有真实 IPv4 的接口」选）。

### 4.2 收尾（修好发送后）
- 清除 `service_daemon.rs` 里的 3 处 DIAG 日志。
- 重新构建 + 安装 APK 到手机。
- PC `serve --discoverable` + `stross devices` 双向发现验证（应能扫到手机 .60）。
- 核对 P0-1 真机闭环（手机→PC 推流）。
- 更新 `docs/iteration-plan.md`。

### 4.3 备选（若发送问题难解）
- 先用「手动输入地址」跑通 P0-1 真机闭环，发现单独修。

---

## 5. 环境 / 命令速查

- 手机 serial：`3B6F5ME8GCL4660T`；WiFi `192.168.11.60`；`stross adb status` 可查。
- CDP（手机 WebView 调试）：`adb forward tcp:19222 localabstract:webview_devtools_remote_<pid>` + `node scripts/phone-cdp.mjs text/click/eval`。
- 抓包脚本（/tmp，Python 原始 socket）：
  - `/tmp/mdns_one.py <iface_ip> <sec>`：按来源统计 QR/记录类型。
  - `/tmp/mdns_qr.py`：判别 QR=0 查询 vs QR=1 应答。
  - `/tmp/qtype.py`：判别手机包的 qtype（PTR=12 vs ANY=255）。
  - `/tmp/q_bysrc.py`：对可选实例发 SRV+ANY，按来源列应答类型。
  - `/tmp/phone_resp.py`：解码手机(.60)的包内容。
  - `/tmp/pc_queries.py`：抓 PC(.61) 发出的查询问题。
  - `/tmp/q_any.py`：对任意实例任一类型查询。
- Android 构建：JDK17（本机可用）`JAVA_HOME=/usr/lib/jvm/java-17-openjdk`，`cargo tauri android build --debug -t aarch64`。

---

## 6. 关键源码位置

- `crates/mdns/src/service_daemon.rs`
  - `prepare_announce`（约 5044-5189）：`intf_addrs.is_empty()→None`（5056）；`probing_count>0→None`（5179）。
  - `announce_service_on_intf`（约 5193-5210）：`apply_multicast_rate_limit` 后 `send_dns_outgoing`。
  - `handle_query`（约 3473-3650）：SRV/TXT 应答依赖 `get_status(if_index)==Announced`（3603）+ `intf_addrs`（3612）→ 否则 `continue`。
  - `handle_read`（接口 fallback，本轮新增）。
  - `ingest_records`（本轮新增）。
  - `my_ip_interfaces_inner`（4796）：`if_addrs::get_if_addrs()` + `!is_p2p()` 等过滤。
  - `send_dns_outgoing`/`send_dns_outgoing_impl`/`multicast_on_intf`（4837/4884/4990）：**待深挖的发送路径**。
- `crates/mdns/src/service_info.rs`：`get_addrs_on_my_intf_v4/v6`（409/417）、`valid_ip_on_intf`（1003）、`set_requires_probe`（267）、`is_addr_auto`（259）。
- `crates/stross-kernel/src/discovery.rs`：`Discovery::start`/`redefine`、`broadcast_addrs`、`select_reachable_ip`。
- `crates/stross-kernel/src/kernel/mod.rs`：`discoverable`/`apply_discoverable`/`try_register_mdns`。
- `crates/stross-transport/src/net.rs`：`local_ips()`（local_ip_address crate）。

---

## 7. 子代理结论（已收到，作参考）

- 子代理第一轮（预告系统）：`set_requires_probe(false)` 有效；A 记录 `matches` 依赖 `interface_id`。
  - 后续被第二轮/实测部分修正。
- 子代理第二轮（补发运行时证据后报告）：结论修正为「**地址记录与 my_intf 匹配失败**（`get_addrs_on_my_intf_v4` 对 wlan0 返回空 → `prepare_announce` 5056 短路），`set_requires_probe(false)` 无效」。
  - **但实测诊断推翻了这点**：`wlan0` 的 `intf_addrs=["192.168.11.60"]` 非空、`prepare_announce` 返回 `answers_count=4`。故地址匹配/探测闸门都不是手机扫不到的真因，**真因落在发送环节**。
- 子代理建议的方案（修 `get_addrs_on_my_intf_v4` 回退显式地址）已在 mdns 测试中导致 `service_with_invalid_addr_{v4,v6}`/`integration_success` 等测试失败（因为「无效地址不可解析」语义被破坏），**已被放弃**。

---

## 8. 根因结论与解决办法（已确认）

### 8.1 一句话根因
**不是 Stross 代码 bug，是 `Su` WiFi 路由器不向手机转发下行 mDNS 多播**（手机能发上行多播、能收单播、能收自己 socket 的回环多播，唯独收不到"从 AP 下来的多播"）。同一套代码在**能正常投递多播的网络**（手机↔PC 经 USB 共享，同网段 10.159.157.0/24）上双向发现完全正常。

### 8.2 关键验证证据（网络加速/双通道已关、TUN 已关）
| 项 | 结果 | 证据 |
|---|---|---|
| PC 出网多播 | 正常 | enp6s0 TX 包计数 +24 |
| 手机收 PC 单播 | 正常 | `recvPkt from=192.168.11.61:48779 if_index=40 intf=wlan0` |
| 手机收 PC 多播 | **收不到** | 加全量入包诊断后 `from=192.168.11.61` 的多播**一次都没到 socket** |
| 手机发多播 → PC | 正常 | PC 抓到手机 `QR=0 PTR`、`QR=1 A+PTR+SRV+TXT` 公告 |
| USB 同网段双端发现 | **双向成功** | PC 扫到手机 10.159.157.104:8777；手机列表出现 PC 10.159.157.158:18777 |

### 8.3 怎么解决（任选其一）
1. **改路由器**（首选，一劳永逸）：登录 `Su`（网关 192.168.11.1），**关闭 IGMP snooping / 关闭 AP(客户端)隔离 / 开启多播转发**，或把手机接到多播通畅的网段。改完手机应能在 WiFi 下被 `stross devices`/对端发现。
2. **用多播通畅的网络**（已验证可用）：手机↔PC 同网段直连（本例用 USB 网络共享 10.159.157.0/24，`rndis0`↔`enp0s20f0u3`），P0-1 真机闭环可稳定跑通。
3. **代码兜底（已实现）**：既然该网络**单播双向是通的**（§8.2 证据），`scan_lan` 增加**子网单播扫描回退**——当 mDNS 浏览零远端设备时，对本机各 /24 网段主机并发单播探测 **18779 `/api/discovery`**（统一发现权威端口），命中即以 `relay_port` 为节点聚合。纯单播、不依赖组播/广播，故在「收不到下行多播」的网络仍能发现/被发现；只在 mDNS 零结果才扫描，避免每次刷新打满网卡。实现见 `crates/stross-kernel/src/discovery/aggregate.rs::{scan_lan, subnet_scan}`。

   > **可被发现门控（隐私）**：该回退受「可被发现」开关约束——`discoverable=false` 时 `/api/discovery` 返回 404（`Kernel::discovery_manifest` 门控），子网扫描探测不到。即「关闭 = 所有发现路径不可见」（mDNS 广播 + 子网单播回退），与用户隐私优先语义一致；不再出现「关了开关仍被局域网发现」的矛盾（用户反馈 bug）。

### 8.4 配套说明
- **排查用诊断日志已清理**（`crates/mdns/src/service_daemon.rs`、`crates/stross-kernel/src/discovery.rs` 中的 `DIAG` 全部移除），恢复为干净的 `trace!`/`debug!`。
- `crates/mdns` 现为本地 fork，保留的**功能性改动**（非诊断）见 §3 与 §6：`set_requires_probe(false)`、`handle_read` 接口 fallback、`ingest_records`（query-with-answer 入库）、`apply_multicast_rate_limit`。
- **待办**：P0-1 真机闭环建议走 USB 网络（已可发现）；WiFi 侧可按 8.3-1 改路由器，或依靠 8.3-3 已实现的子网单播扫描回退（`scan_lan` 在 mDNS 零远端时自动触发）。

---

*记录时间：已排查完毕（结论见 §8）。上一记录为排查现场备忘，本次保留为带证据的结论归档。*
