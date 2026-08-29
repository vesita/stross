# 运行闭环整改计划（开发期）

> 状态：2026-09 启动。**P0-1 端点共享生命周期已落地（第十二轮，见 iteration-plan.md）**；
> P0-2 / P1 / 协议优化排后续轮次。范围声明：聚焦**运行闭环**缺陷修复与**协议自身优化**；
> **不维护新旧版本 wire 兼容**（开发期全端同步演进，wire 可破坏性变更）。
> 每项改动必须能追溯到缺陷编号（下表）；验证偏好见 AGENTS.md §7。

## 1. 现状问题清单

| # | 环节 | 缺陷 | 严重度 | 处置 |
|---|---|---|---|---|
| 5 | 通告/共享 | 端点共享一旦被订阅启动，公开方无停止入口；订阅者断开后不自动收尾；取消通告不停止推流 | 高 | **本轮 P0-1** |
| 6 | 订阅 | 同一端点第二订阅者必然失败（单引擎 + 每次订阅新建会话），"一人通告多人看"不成立 | 高 | **本轮 P0-1** |
| 7 | 共享 | 单引擎限制：本机同时只能一个推流（多端点并发共享不可行） | 中 | P0-1 缓解（收敛+拒绝）；引擎多路留待评估 |
| 8 | 共享 | 端点 state/subscribers 从不更新（`set_state` 生产代码零调用） | 中 | **本轮 P0-1** |
| 9 | 会话 | 会话/凭证/授权三表只增不减（无 teardown 接线、token 惰性清理不完整） | 高 | **本轮 P0-1** |
| 14 | 接收 | 断线自动重连完全缺失（验收 4 未达成） | 高 | P0-2（下轮） |
| 16 | 接收 | 断流原因不区分（"对方停止"vs"网络异常"，stats.error 中途不置位） | 中 | P0-2 |
| 15 | 推流 | 推流拒绝原因被吞（Hello Error → connect 报"Welcome 超时"误导） | 中 | P0-2 |
| 3 | 发现 | 内核事件桥建了前端不用，全靠 2s/5s 轮询（延迟 + 扫描风暴） | 中 | P1 |
| 2 | 发现 | 设备无"离线"中间态（消失即删除、展开状态被清） | 中 | P1 |
| 13 | 订阅 | 协商等待无进度（首见 Confirm 最长干等 70s） | 中 | P1 |
| 12 | 端点 | load 不重探测（权限授予后端点仍不可用） | 中 | P1 |
| 4 | 锚定 | 端口回退随机后防火墙规则失配（有自检兜底） | 低 | P1 |
| 1 | 发现 | mDNS 跨设备不可靠未根治（Android 组播间歇静默） | 中 | P2（纯净网络复测） |
| 19 | 文件 | 文件泵重试靠中文错误串匹配判断 | 低 | P2 |
| 18 | 数据面 | watch 无鉴权 + stream_id 可枚举 + `/api/proxy` 无鉴权 | 高 | **协议优化阶段** |
| 17 | 数据面 | 静止画面保活依赖推流端自觉（无应用层保活控制帧） | 中 | **协议优化阶段** |
| 21 | 数据面 | pts_ms u32 回绕（49.7 天）、frame len 回绕未 checked_add | 低 | **协议优化阶段** |
| 11 | 安全 | 信任/Private 白名单基于自报 device_id（无密码学） | 中 | **协议优化阶段**（远期） |
| 10 | 订阅 | push 方向 relay_addr 取第一个非 fake IPv4（多网卡可能不可达） | 中 | P1 |

明确不做：新旧版本 wire 兼容/降级（#20，用户指示）；浏览器观看端 / 云中继 / GStreamer（YAGNI）。

---

## 2. P0-1 端点共享生命周期（本轮实施）

### 2.1 目标

1. **端点共享可停止**：公开方 GUI 一键停止；取消通告联动停止。
2. **订阅者全部断开后自动收尾**：watchers=0 保持一段时间即停流 + 拆会话。
3. **同端点多订阅者收敛**：pull 复用同一流（中继多 watcher 真正生效）；push 单订阅限制（清晰报错，不再静默失败）。
4. **会话/凭证/授权随流结束清理**：StreamEnded → 本机会话自动 teardown；teardown 同时清 token。
5. **端点 state/subscribers 实时更新**：set_state 接线（Active/Idle + watchers 计数）。

### 2.2 设计

**数据结构**（`Kernel`）：

```rust
struct ActiveShare { endpoint_id: String, delivery: Delivery }
// Kernel.active_shares: Mutex<HashMap<String /*stream_id*/, ActiveShare>>
// Kernel.share_stop_delay: Duration      // watchers=0 停止延迟（默认 4s，测试注入）
// Kernel.share_idle_delay: Duration      // 无 watcher 接入窗口（默认 10s，测试注入）
```

**登记路径**（端点自驱动，零内核类型分派）：

- `stross-endpoint::contract::spawn_media_share` 增加 `endpoint_id: &str` 参数
  （调用点 3 处：`screen/mod.rs` / `audio.rs` Mic / `audio.rs` SystemAudio）；
- `EndpointApp` trait 新增 `note_share_active(endpoint_id, stream_id, delivery)`（默认空实现，
  file 端点不登记——它有完成态，StreamEnded 统一清理会话）；
- `spawn_media_share` 在 `start_stream` 成功后调用 `note_share_active` → Kernel 登记 +
  `set_state(Active, 0)` + **spawn 接入窗口检查**（`share_idle_delay` 后复查 watchers 仍 0 →
  停流，覆盖"订阅者从未接入"场景，与事件顺序无关）。

**停止路径**：

| 触发 | 实现 |
|---|---|
| 显式停止（GUI 按钮） | `Kernel::stop_endpoint_share(endpoint_id)`（async）：查登记 → `stop_stream`（优雅 Bye）→ `teardown` → 清登记 → `set_state(Idle, 0)`；GUI 命令 `endpoint_stop_share` |
| 取消通告 | `unpublish_endpoint` 改 async：若该端点有活动共享 → 同上停止后再翻 published |
| watchers→0 | `attach_data_plane` 转发任务捕获 `WatchersChanged{w:0}` → stream_id 命中登记 → spawn 延时（`share_stop_delay`）→ 经 `DataPlaneBackend::stream_watchers` 复查仍 0 → 停止 |
| 流意外结束（推流端断开/静默超时） | 转发任务 `StreamEnded` 分支：清登记 + `set_state(Idle,0)` + 本机会话 `teardown`（远程 push 会话 SessionNotFound 忽略） |

**订阅收敛**（`negotiator.rs`）：

- `compose_grant` 端点语义分支先查 `app.active_share_by_endpoint(eid)`：
  - 命中且定稿 delivery == Pull → **复用现有 stream_id** 构造 grant（不新建会话/凭证；
    `view` 只带 stream_id，pin/token 置空——pull 订阅方只用 stream_id）；
  - 命中且定稿 delivery == Push → 返回错误
    「端点当前正被其它订阅者使用（push 一次仅一个订阅者）」；
- **复用时不重复触发 share**：`notify_subscribed` 调用点（handle_request / respond）改为
  「无活动共享才触发」；复用场景由中继 watchers 自然计数（`WatchersChanged` → 反查登记 →
  `set_state(Active, watchers)`）。

**会话清理**：

- `Kernel::teardown` 补充移除 `share_tokens` 表项（此前遗漏）；
- `StreamEnded` → 若 `has_session(stream_id)` 则 `teardown`（无 PIN 会话直接放行）。

### 2.3 边界与竞态

- watchers=0 与"订阅者尚未接入"竞态：`share_stop_delay`（4s）覆盖 watch 建立延迟；
  `share_idle_delay`（10s）覆盖"从未接入"。
- **复用以登记存在为准**（不做本地流存在性复查）：登记在停止/流结束时同步清除，
  pull 本地流与 push 远端流（公开方登记、流在订阅方中继）都被覆盖；
  "停止与新订阅者接入之间"的极端竞态（登记已清 → 走新建路径，天然自愈），
  残余窗口由 P0-2 重试兜底。
- 单引擎：pull 复用后第二订阅者不再触发 `start_stream`；push 拒绝 → 无冲突。

### 2.4 测试计划

- kernel 单测：登记/清除/`stop_endpoint_share`；teardown 清 token。
- negotiator 单测：pull 双订阅复用同 stream_id；push 第二订阅拒绝；复用不重复触发 share。
- 进程内双节点：双订阅者同流都收到帧；订阅者断开 → 延迟后流回收（注入短延迟）。
- 前端 jsdom：本机端点树「停止共享」按钮出现/点击断言。
- `scripts/check.sh` full 全绿（fmt / clippy -D warnings / workspace 测试 / tsc / jsdom）。

---

## 3. P0-2 断线自动重连（下轮）

- 接收端：区分错误类型——流不存在（404，短暂重试 N 次后放弃）/ 瞬时错误（指数退避重连，
  上限 N 次）；重连后重发 watch（中继关键帧缓存保证可接入）；`stats.error`/结束原因字段
  区分「已结束 / 网络异常」。
- 推流端：`client_loop` 断连后自动重连（重新 Hello，携带原凭证），`connected` 状态上抛。
- 推流拒绝原因透传（#15）：relay Error 消息经 watch channel 作为 connect 错误返回。
- UI：接收面板「重连中…」状态。

## 4. P1 体验（后续轮次）

- 前端消费 `kernel-event` 驱动增量更新，轮询降频（#3）；
- 设备离线灰态（#2）；订阅等待倒计时 + 取消（#13）；
- load 重探测：权限授予/环境变化后端点 reload（#12）；
- push relay_addr 多地址宣告或按请求方网段选择（#10）。

## 5. 协议优化（开发期，wire 自由演进）

> 不做兼容，wire 可破坏性变更；按优先级排：

1. **watch 鉴权 + stream_id 不可枚举**（#18）：watch 请求携带订阅关系凭证；会话 id 改随机
   hex（不可枚举）；`/api/proxy` 限回环或鉴权。
2. **应用层保活控制帧**（#17）：推流端定期发送 Ping/静默帧（协议级心跳），替代
   "重发上一帧"依赖，SRT/QUIC/Android 路径统一。
3. **pts_ms 回绕处理 / frame len checked_add**（#21）。
4. **device_id 挑战应答**（#11，远期）：信任与 Private 白名单加密码学绑定。

## 6. 验证与提交

- 每轮改动：`cargo test -p <crate>` → `scripts/check.sh full` → 真机路径 adb+CDP 实测，
  结论记入 `docs/iteration-plan.md`；
- 提交：用户确认后 single commit，`fix(scope): 行为差异` 风格（AGENTS.md §7）。
