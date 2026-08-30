# Stross 迭代方案（2026-09 起）

> 输入：`requirements.md`（v2 定稿，一期 6 条验收）+ `roadmap.md`（P0 完成现状）。
> 方法论约定（沿用）：任何迭代任务必须能追溯到某条需求/非功能指标；任何架构决策
> 必须能追溯到某条需求。
> 本方案为**迭代顺序建议**，每阶段独立可交付、可验收；顺序可按实测反馈调整。

## 0. 参考开源项目（检索 2026-09）

| 项目 | 场景 | 借鉴点 | 对齐 Stross |
|---|---|---|---|
| [scrcpy](https://deepwiki.com/Genymobile/scrcpy/1.1-architecture)（Genymobile） | 手机镜像/反向外设标杆 | 多通道分离（video/audio/control 独立 socket）；client-server 握手含版本+能力协商；低延迟音频实现（小采集缓冲 + 直接写 AudioTrack） | §4.4 流式/无损双通道；跨设备协商消息；B 阶段反向音频延迟预算 |
| [QuicMic](https://github.com/Fix3dll/QuicMic)（Rust + QUIC） | **手机当 PC 麦克风**（浏览器零安装） | 与 Stross D3 同场景、同传输栈（QUIC）；一次性接入凭证模式（链接/扫码进入，免配置） | B1 凭证式接入设计；QUIC 纯音频低延迟路径 |
| [WO Mic](https://github.com/backmactep9332/wo-mic-premium-verison) | 手机当电脑麦克风（经典） | PC 端虚拟音频设备集成：Windows 虚拟麦克风（WASAPI）/ Linux 虚拟源（ALSA/PipeWire）；手机采集→传输→PC 注入系统录音设备 | B4 音频 Sink 二期增强：从"扬声器播放"升级为"注入录音设备" |
| [Sunshine](https://baike.baidu.com/item/Sunshine/67912019) / Moonlight | 游戏串流（低延迟） | 动态码率/分辨率自适应；FEC 前向纠错；延迟/丢包遥测上报 | C2 弱网韧性；F5.3 运行页延迟/帧率实时显示 |
| [jittr](https://packages.ecosyste.ms/registries/crates.io/packages/jittr)（crates.io） | 音频抖动缓冲库 | 定长环形缓冲 + 按 seq/pts 排序 + 超时跳过策略；Opus 场景 20–100ms 自适应 | B5 抖动缓冲（需求 §4.4 已有设计，算法可对照） |
| [Castify](https://github.com/sh1zen/Castify)（Rust + gstreamer） | 跨平台投屏 | 传输层替代方案参考（我们已定 ffmpeg 编排，仅作对照，不引入） | — |
| [弱网测试框架](https://datasea.cn/go0808695146.html)（netem+tc） | 弱网仿真 | tc netem 丢包/延迟/抖动脚本化 + 结果断言；用户态 TCP 栈协同 | C1 弱网测试自动化（补已知问题③） |

YAGNI 边界（沿用 requirements §9）：不做浏览器观看端、不做云中继、不引入
GStreamer 流水线。

---

## 阶段总览

| 阶段 | 名称 | 对齐需求 | 核心交付 | 状态 |
|---|---|---|---|---|
| A | 当前收口 | 一期验收 1/5 | 已知问题②④ + 三设备实测 + 提交 | ✅ 已完成（A0–A3 落地；A4 三设备实测部分完成，见轮次记录） |
| B | **反向外设（D3 核心验收）** | 一期验收 3、6；§6.1 反向音频 ≤200ms | 跨设备推流：手机麦克风 → 电脑播放 | ✅ 核心闭环完成（B1/B2/B2.5 真机闭环；B3/B4/B5/B7 落地；B6 部分） |
| C | 弱网与长跑韧性 | §6.2 弱网 5–10% 丢包不冻结 >1s、内存有界 | 弱网测试自动化 + 韧性手段 + 长跑验证 | ⏳ 待开始（弱网基线已测） |
| D | 多端互通与网格完善 | 一期验收 2、4、6；F5.1/F5.3 | Android GUI、互通矩阵、会话控制完善 | 🔄 进行中（D1 Android GUI 已落地；互通矩阵/传输统计待办） |
| E | 二期无损共享 | F6.1/F6.2 | 文件互传 + 剪贴板同步（ReliableChannel） | 🔄 二期（文件端点已落地；剪贴板待办） |

---

## 阶段 A：当前收口（0.1.1）

> 对齐：一期验收 1（互发现）、5（移除浏览器后链路完好）。把已完成的 P0 工作固化交付。

- [x] A0 本地开发自动化（已完成：check.sh / hooks / build.sh / 双设备测试）
- [x] A1 提交当前工作区（等用户确认；single commit：conventional 前缀 + 中文描述）
- [x] A2 修复多网卡 mDNS 广播（已知②）：`Discovery::start` 改为接收全部
      局域网 IP 一次注册（mdns-sd `AsIpAddrs` 多地址记录），4 处调用点
      （app.rs / stross-relay / cli relay / gui relay-only）全部改为广播
      全部 IP；空列表回退回环；`broadcast_addrs` 纯函数 + 3 单测
      （多 IP 全广播 / 单 IP 透传 / 空回退回环）
- [x] A3 清理 docs 旧浏览器架构残留（已知④）：architecture.md（§0 交互
      模型→免先连网格、§1 数据流→原生接收端、§5 观看端→接收端播放、
      §8 决策表）、protocol.md（HTTP 表删 `/`、`/app.js`、`/jmuxer.js`，
      补 /api/info、/api/peers、/api/proxy、/api/proxies；术语统一为
      "接收端"）、plugin-architecture.md、roadmap.md P1 同步
- [~] A4 三设备实测（roadmap P0 最后一项）：A 推流 → B 直连看；C 跨网段经 B
      中继级联看（真机；单机双实例已覆盖逻辑，重点验证 mDNS 跨网卡与
      SRT/QUIC 跨设备拨号）
      **2026-08-26 真机进展**：OPPO（Android 16）作为设备 B 完成「发现（双向 mDNS）
      → 连接（SRT 直连，watchers=1）→ 观看（Canvas 渲染）」，并暴露/修复：
      frontendDist 数组导致 Android 前端资源缺失、mDNS 广播/浏览混入 fe80
      link-local（点不开的卡片）、Android 观看统计解码计数不回写、状态文案卡死。
      待办：C 设备级联观看未测；手机麦克风反向推流（阶段 B2）未测。
- 验收：A2 后多网卡设备在网格中被各网卡网段正确发现；A4 三设备拓扑全通。

## 阶段 B：反向外设——跨设备推流（D3，一期核心验收）

> 对齐：一期验收 3（**电脑选择"接收手机麦克风"→ 确认 → 电脑扬声器/录音设备
> 播放手机声音**）；§6.1 反向音频 **端到端 ≤ 100–200ms**（一期最严苛指标）。
> 参考：QuicMic（凭证接入）、scrcpy（多通道/低延迟音频）、WO Mic（录音设备注入）、
> jittr（抖动缓冲）。

### B0 关键设计决策：凭证式跨机会话协商（推荐）

现状约束（roadmap 记录）：受控中继只接受内核授权会话 id（F2.2），`/api/*` 无
建会话/授权端点 → 跨设备推流需"跨机会话协商"。

设计判断：**D3 场景中会话由接收端（电脑）发起，不需要"远程控制电脑"**——
是电脑主动选"接收手机麦克风"，手机只是把麦克风数据推过来。因此协商走
**凭证式接入**，零新增监听端点、零远程控制面暴露，安全模型与 D7 一致：

1. 电脑 GUI/CLI「接收手机麦克风」→ 电脑内核**建会话并授权**该 stream_id
   （复用 F2.2 现有机制，无协议改动）；
2. 电脑生成**一次性接入凭证**：`{stream_id, 接收端中继地址(srt/quic/ws), PIN, 短时效}`
   ——借鉴 QuicMic 的链接/扫码接入 + F2.5 会话级 PIN；
3. 凭证经二维码/短码展示（或用户在手机网格页手动输入地址）；
4. 手机用凭证**直接向接收端受控中继推流**——中继侧校验已授权 stream_id，
   现有逻辑零改动；
5. 手机推流开始后，电脑原生接收（现有 watch 链路）→ 音频 Sink 播放。

备选路线（B0'，不推荐现阶段做）：手机→电脑先建 WS 控制连接再协商——
需要电脑控制面开放 LAN 绑定，违背 D7 v1"控制面仅回环"的门控，留待
"远程控制（手机控制电脑）"阶段再评估。

### B1 凭证生命周期（stross-proto 扩展）✅

- [x] `ShareToken` 结构（v/stream_id/pin/expires_at/media）——平台无关 JSON，
      可入二维码/短码；`to_token_string`/`from_token_string`/`is_expired` +
      单测（roundtrip/容错/过期边界）
- [x] `ControlMessage::Hello` 增加可选 `share_token` 字段（向后兼容：旧推流端
      不传、wire 不变）；单测含旧客户端解析
- [x] 内核签发与校验：`Kernel::create_share_token(session_id, media, ttl)` /
      `verify_share_token`（签发表 + 过期惰性清理 + 逐字比对防篡改/重放；
      PIN 为伪随机 6 位，不引入 rand 依赖）；单测覆盖签发/校验/篡改/过期/
      同会话重签覆盖
- [x] 中继接入校验（来源感知门控）：`DataSession::peer_addr()`（ws/srt/quic
      实现；QUIC 直接从 connection 派生）→ 受控中继**回环来源走内核预授权
      （本机流程不变），非回环/未知来源必须出示有效凭证**——预授权不再能
      被远程冒用；`ShareTokenValidator` 钩子由内核 attach 数据面时注入
- [x] 控制面与命令面：`ctrl share-token <session-id> [--ttl N]` 签发并打印
      token；`push --share-token <token>` 出示凭证推流（Hello 携带）
- [x] 测试：kernel 单测 + 集成测试（凭证放行/篡改拒绝/过期拒绝/非回环
      即使已预授权也须凭证）+ **双 PC 端到端脚本**
      `scripts/share-token-test.sh`（PC-A serve+签发 → PC-B 凭凭证经局域网
      IP 推流 → PC-A 播放；含两个反例）

### B2 手机端反向外设推流（Android）✅ 真机闭环验证通过

- [x] **手机麦克风纯音频采集模式**：`MediaPlugin` 新增 `micOnly`——跳过屏幕
      录制授权/前台服务/虚拟显示，只走 AudioRecord→AAC（授权失败/初始化失败
      回传 t=9 状态帧，无屏幕兜底）；Rust `mobile.rs` 依 `cfg.video.is_none()`
      传参（延续 B7 Rust 化方向）
- [x] **凭证推流不改写会话 id**：`ensure_session` 对携带 `share_token` 的推流
      一律跳过兜底改写——stream_id 是接收端签发，改写会把 id 换成新会话导致
      接收端收不到（回归保护）
- [x] **手机端 GUI（组合管理界面内）**：设备卡片「共享麦克风到 TA」→ 凭证
      输入弹窗 → 解析 ShareToken 取 stream_id → 推流；目标地址 QUIC 优先
      （`/api/info`），无 QUIC 回退 WS push
- [x] **电脑端 GUI 签发与自动接收**：本机卡片「接收手机麦克风」→ 内核
      `issue_share_token`（建会话+签凭证，默认 600s）→ 展示 PIN+凭证（可复制）
      → 轮询本机 `/api/streams`，流接入即自动原生接收（`audio=device` 扬声器）
- [x] **真机闭环验收（2026-08-26，OPPO PLC110 ↔ 本机）**：手机 GUI 手动添加
      电脑设备 → 共享麦克风到 TA → 粘贴凭证 → **QUIC 直推**电脑受控中继
      （纯音频 QUIC>WS 自动选择生效，serve 日志 `QUIC 入站连接 192.168.11.60`
      + `推流开始: sess-2`）→ 电脑端原生接收 **290 帧 / 音频块 289/289 /
      缓冲丢弃 0**——D3「电脑接收手机麦克风」数据链路真机闭环完成
- [x] **凭证自动协商（B2.5 权限自动化，真机验证通过）**：手机对设备点
       「共享麦克风到 TA」→ `POST /api/negotiator/request`（携带 device_id/name）
       → 电脑 GUI 首次人工确认（可记住设备）→ 签发一次性凭证 → 手机自动
       QUIC 推流、电脑 GUI 自动接收；信任设备免确认自动签发；手动粘贴兜底
       （对端无协商端点时自动回退）。真机：电脑 `QUIC 入站连接 192.168.11.60`
       → `推流开始: sess-1 (手机麦克风)` → watchers:1 电脑自动接收；
       `trusted_devices.json` 持久化手机 device_id 生效
- [~] 剩余真机事项：**B7 解码帧率追平复测**（本次未测屏幕→手机观看）

> 真机暴露的环境问题（记录，后续处理）：
> 1. **Android mDNS 浏览在 USB 网络共享（vgate0 为默认路由）场景收不到
>    局域网广播**——已加 MainActivity `MulticastLock`（OPPO ColorOS 默认
>    拦组播），本场景仍未发现：疑默认路由走 vgate0 影响组播路径，待接
>    mdns-sd 接口指定/网络场景复测；
> 2. **电脑 ufw 需放行局域网**（`sudo ufw allow from 192.168.11.0/24`）——
>    **已自动化为「固定端口 + 自检 + polkit 一键放行」**，见「阶段 B 附」；
> 3. **QUIC 硬断连（force-stop）流残留——已修复（2026-08-27）**：
>    根因 quinn 默认 idle 30s 且无 keepalive，对端 force-stop 后流残留约半分钟；
>    修复=服务端 idle 15s + 客户端 keepalive 10s（crates/stross-transport
>    quic.rs，静默观看连接由 keepalive 续命、死连接 15s 判死）；
>    回归=`scripts/quic-stale-stream-test.sh`（SIGKILL 推流端 → 流 16s 内从
>    /api/streams 移除）+ `hard_disconnect_released_by_idle_timeout` 单测。

## 阶段 B 附 2：CLI 状态命令（2026-08-27，调试工具链）

> 对齐：F5.3（运行状态）/ 无头调试需要。方向"CLI 优化——更好地获取 PC 与手机
> 运行状态"的三条命令，互补覆盖两种通道：

- [x] `stross devices` —— **局域网设备状态**（mDNS 扫描，无需 serve 常驻）：
      设备名 / 角色 / 可共享媒体 / 传输 / SRT·QUIC 端口 / 在线共享（含 watchers）；
      本机去重（按 mDNS 实例名，A/AAAA 各一次 resolved 只留 IPv4）+ 本机回环
      探测；`--json` 脚本化；0 台时提示 `stross adb status`
- [x] `stross ctrl status` —— **本机实例状态**：`CtrlRequest::Status` 响应扩容
      （version / platform / uptimeSecs / srtPort / quicPort / streamId+Title+
      StartedAt），人类可读表格输出（`--json` 保留原始 JSON）
- [x] `stross adb {status,screenshot,ui-status,tap,swipe,type,key}` —— **经 USB 的
      手机状态与无头交互驱动**（局域网 AP 隔离/mDNS 不可达时的可靠通道）：
      a. `status`：型号 / Android 版本 / WiFi IP / 中继端口（`adb forward`
         直通手机中继 `/api/info` + `/api/streams`，**不用 reverse**——本环境
         adb reverse 注册但不生效，forward 稳定）；
      b. `screenshot`：`adb exec-out screencap` 截屏到 PNG；
      c. `ui-status`：截图 + `uiautomator dump` 视图树文本化（WebView 页面
         文本在多数系统可见）——一行命令看手机 UI 在显示什么（调试用）；
      d. **`tap [文本|--xy]` / `swipe` / `type` / `key`**：按视图树文本精确点按
         （自动取元素中心）或直接坐标，配合 ui-status 形成「看状态→动手→
         再看状态」的无头交互回路；真机验证：`stross adb tap "扫描"` 命中
         (238,496)
- [x] 真机实测（OPPO PLC110，新构建 0.1.0）：`stross adb status` 展示
      192.168.2.24 手机中继 8777 + SRT 33462 + QUIC 33464；`ui-status` 识别出
      「本机入口 → 读取中…」卡占位——暴露 **UI bug：设备列表重建清空 ip-list
      后不重渲染**，已修复（renderDeviceList 统一重渲染 IP 列表）
- [x] **跨设备 mDNS 发现故障检视（2026-08-27，未根治，证据已归档）**：
      `stross devices` 扫不到手机但能扫到本机；`stross_kernel::discovery::browse`
      已加 `accept_unsolicited(true)`（与 GUI 配置对齐；效果：ServiceFound 从
      「完全不触发」变为「可触发」，但手机实例仍不 resolve）。关键证据：
      a. 裸 python socket（SO_REUSEPORT + 加入 wlan0 组）能收到手机 PTR 响应，
         说明手机广播/PC 收包链路可用；
      b. fork trace：手机 PTR 记录被解析并准入缓存（无 not-for-us），但
         ServiceFound 只对 self 实例触发；resolve 从不调度（`exec_command:
         Resolve` 计数 0）；
      c. TEMP-DIAG（service_daemon.rs process_packet，临时留档）：手机 mDNS
         流量是**间歇性**的——应用重锚定后发一小段即静默（Android 组播被
         挂起/vgate0 默认路由），且手机包表现为「1 问 + 1 答（自身 PTR 为
         known-answer）的查询」，走 handle_query 时答案段被丢弃；
      d. 环境干扰：PC 上有其它 5353 绑定者（systemd-resolved 等）与 Clash TUN
         fake-IP（198.18.0.1），可能分摊组播。
      结论：跨设备 mDNS 在现网（手机间歇静默 + 组播路径受默认路由/系统
      服务干扰）不可靠，需在**纯净网络（手机热点）**下用带临时诊断日志的
      fork 复测定位；此问题与「隐患 #1（Android 组播/默认路由）」同源。
- [ ] （后续）`stross adb` 窗口坐标上报 + 自动重试，做无头交互驱动的完整
      真机回归脚本

### B3 电脑端音频输出 Sink（PlaybackSink）

- [x] `PlaybackSink` trait 桌面实现（D6 已定）：ffmpeg 子进程解码
      AAC→PCM + cpal 输出扬声器；与采集侧同一二进制、零新增原生依赖；
- [x] 播放路径接入现有 watch 链路：接收音频帧 → 解码 → 扬声器；
- [ ] （二期增强，WO Mic 路线）录音设备注入：Windows WASAPI 虚拟麦克风 /
      Linux PipeWire 虚拟源——"录音设备播放手机声音"的完整版；
- 验收：电脑扬声器听到手机麦克风声音（D3 验收 3，CLI 双端已验证）。

### B4 反向音频低延迟路径（≤100–200ms 预算）✅ 测量设施，待 SRT 调参

> 参考：scrcpy 低延迟音频（小缓冲 + 直接写）、QuicMic QUIC 音频、jittr 自适应抖动。

- [x] **延迟测量设施固化**：`push --report-start`（首帧墙时刻 + pts0 修正，
      排除 ffmpeg 预热）+ `receive --calibrate`（同钟绝对端到端延迟
      min/p50/p95/p99）+ `receive --no-write`（长跑零 IO，避免落盘污染）
- [x] **双 PC 实测**（`scripts/latency-stability-test.sh [SECS] [trans...]`，
      45s 长跑 + 多传输对比）：
      - **QUIC：端到端 min=0.9ms / p99=4.1ms**（本机局域网，≪200ms 达标）；
        相对抖动 p99=1.3ms；帧 94.7% / 音频块 95.5%（缺量=接入窗口，非掉帧）；
        RSS 30.5→32.2MB 有界
      - **SRT：min=240.9ms / p99=245.0ms**——系统性延迟（分布极窄，非抖动），
        rsrt `SrtOptions.latency`（SRTO_RCVLATENCY 默认 120ms × 两级链路 ≈240ms）；
        **待调参**（如 40ms）后再测；局域网低延迟优先 QUIC
- [ ] 编码低延迟参数（AAC/Opus）：QUIC 路径已 ≤5ms，非瓶颈；仅当 SRT
      调参后仍不达标再评估
- 验收：QUIC 路径端到端 ≤ 200ms 已实测达标（脚本可回归断言）

### B5 接收端抖动缓冲（SessionDataManager 流式通道）✅

> 基础（定长环形 / 乱序落槽 / 空洞超时跳过 / 视频关键帧重对齐 / 音频跳洞 /
> 内存有界断言）已随需求 §4.4 骨架存在；本轮补齐自适应与链路接入。

- [x] **自适应等待窗口**（jitter.rs）：到达间隔 EWMA 抖动估计 → 空洞等待窗口
      动态取 `[min_wait, max_wait]`——抖动小收紧（低延迟）、抖动大放宽（防
      卡顿）；单测：低抖动收紧 / 高抖动放宽 / 窗口内空洞等待后补发
- [x] **音频轨收紧**（session_channel.rs）：音频 `max_wait=100ms` +
      `min_wait=10ms` + 自适应（≤100ms 预算，需求 §4.4）；视频 200ms 保持
- [x] **接收链路接入**（receiver.rs）：`channel_kind_for` 按传输分流——
      SRT（Adaptive，可能乱序/超时丢）→ `Lossy` 进抖动缓冲；WS/QUIC（全序
      不丢）→ `Lossless` 直通零延迟；`receive_loop` 与 `receive_raw_loop`
      都接入
- [x] **推流端 seq 分配**（sender.rs，隐藏前置）：`RelayClient` 发送点统一
      分配会话内递增 seq（此前恒 0，有损路径 jitter 无法工作）；SRT 裸推流
      测试同步补 seq 填充
- [x] 回归：全量测试 + clippy 干净；`dual-device-test.sh`（SRT 推流路径）与
      `share-token-test.sh` 全绿

### B7 Android 播放链路 Rust 化（解码跟不上接收的根治）✅ 代码落地，待真机复测

> 真机暴露问题：Android 观看端解码速度远跟不上接收——Kotlin 命令线程同步
> 解码 + 纯 Java 逐像素 YUV→RGBA + `dequeueInputBuffer(5_000)` 可能阻塞 5s +
> 事件载荷是 serde 序列化的 51.8 万元素 JSON 数组（~2.5MB/帧），四重瓶颈叠加。

- [x] **stross-media 新增 `yuv.rs`**：YUV420（NV12 半平面 / I420 平面）→ RGBA
      最近邻缩放纯函数 + 4 单测（两布局同色一致 / 缩放保比例 / 非紧凑 stride /
      非法输入拒绝）——替代 Kotlin `sendRgbaFrame` 60 行逐像素 Java 循环
- [x] **stross-media `nal.rs` 新增 `extract_avc_config`**：从关键帧 Annex-B 提取
      csd（SPS+PPS）+ 尺寸 + 3 单测——替代 Kotlin `BitReader`/`parseSpsDimensions`
      ~120 行位级解析（Rust `sps_dimensions` 早已实现，Java 重复造轮子）
- [x] **Kotlin `PlaybackPlugin` 瘦身为 MediaCodec/AudioTrack 薄壳**：
      `feedVideo` 入队立即返回（命令线程不再被解码拖住）；独立解码线程 +
      有界队列（满时优先丢非关键帧、关键帧清队重入对齐）；`dequeueInputBuffer`
      短超时（2ms，忙即丢帧，不再阻塞 5s）
- [x] **JNI 直传（stross-gui `mobile_jni.rs`，jni 0.21 = tauri 锁定版本）**：
      Kotlin 解码线程持 YUV 调 `nativeSubmitYuvFrame` → Rust 转换缩放 + base64
      事件 `receive-frame` + 解码统计回写，零 base64/JSON 往返
- [x] **事件载荷 base64 化**（桌面 + Android 统一）：替代 serde 对 `Vec<u8>`
      输出每字节一个数字的 JSON 数组（518KB RGBA → ~2.5MB/帧）——base64
      字符串 ~4 倍紧凑，前端 `atob` 原生解码
- [x] **接收侧积压跳帧**（mobile.rs）：消费循环超过 `DROP_BACKLOG` 阈值时
      丢非关键帧追实时（关键帧/配置帧绝不跳）——修复 mpsc(32) 满丢**新**帧
      导致"永远吃旧帧、滞后累积"的劣化
- [x] 回归：桌面全量测试 / clippy（桌面 + Android 双目标）/ fmt / 前端 jsdom /
      双设备端到端全绿；Android 目标编译通过（NDK 28 clang，本机 toolchain）
- [ ] 待办：真机复测「电脑屏幕 → 手机观看」解码帧率追平（目标 ≥ 接收帧率，
      对比旧版「解码落后 30%+」）；Phase B2 手机反向麦克风推流闭环顺带验证

### B6 会话控制完善（F2.3/F5.3）

- [~] 运行中可停止共享：**界面改版后右栏「共享流」面板统一提供停止按钮**
      （出站广播/定向 `stop_stream`，入站接收 `stop_receive`）——单项共享
      可独立停止；断开会话（内核 teardown）仍未接 UI
- [ ] 运行页实时状态：延迟/帧率/缓冲水位（传输统计推送，D5 已有基础；
      当前共享面板展示帧数/音频块/推流时长，延迟与水位待接）
- 验收：一期验收 4（运行中可停止单项共享、断开会话）。

## 阶段 B 附：界面改版——「设备 × 共享流」组合管理（2026-08）

> 需求 F5.1「PC/手机交互逻辑一致；扫描 → 选共享 → 选接收 → 建立」。

- [x] **信息架构抽象（替代原三 tab：网格/推流/观看）**：设备是实体，共享流是
      设备之间的连接实例；双栏视图——左「设备」（本机 + 局域网设备卡片，
      点设备展开：发起共享 + 该设备的在线共享点即收），右「共享流」（本机
      全部活动共享统一管理：方向 ↑↓ / 媒体 / 对端 / 状态 / 停止）
- [x] **双向共享语义统一**：出站（本机共享屏幕/麦克风，「广播」锚定本机 +
      「定向」B2 凭证直推设备）与入站（点设备在线共享条目即接收）在同一
      设备卡片层操作，原「推流页/观看页」功能折叠进设备层，语义一致
- [x] **广播推流归入本机卡片**：共享屏幕（弹窗选音频：麦克风/系统声/画质）
      与共享麦克风（纯音频，桌面 ffmpeg / Android micOnly）均为「本机 → 局域网」
- [x] 回归：jsdom 交互测试全绿（24 项断言：锚定/设备渲染/展开/点流即收
      UDP 优先/广播共享/手动添加/B2 凭证双向）；desktop+Android 编译、
      clippy、全量测试、双设备端到端无回归
- [x] **防火墙放行的智能收窄（B2 真机暴露：ufw 默认 DROP 入站拦截跨设备
       推流）——已落地为「权限自动化」（B2.5）**：
       a. ✅ **端口固定化**：SRT/QUIC 默认固定 33462/33464
          （`start_relay_fixed`，被占用回退随机并按**实际端口**生成放行
          规则）+ 中继 WS 固定（GUI 8777 / CLI serve 18777）+ 协商端点
          18779 → ufw 只需放行已知端口，不再放行整个网段；
       b. ✅ **自检 + 一键放行**：`firewall_status` 只读执行 `ufw status
          verbose` 解析缺失放行（普通用户可执行，GUI 启动即静默自检，无
          缺失不打扰）；`firewall_allow` 经 polkit（`pkexec`）弹**一次**
          系统授权框，自动添加精确规则（`allow from 192.168.11.0/24 to
          any port 8777,18779 proto tcp` + UDP 段）——不再手敲 sudo，
          规则持久化（/etc/ufw/user.rules）后不再询问；
       c. ✅ 安全定位：应用层**受控中继 + 凭证门控**才是主防线（B1 来源
          感知门控：非回环必须凭证），防火墙放行是网络可达性层，两者职责
          分离——协商端点（凭证柜台）只签发短时一次性凭证，首次人工确认 +
          信任记忆；
       d. （远期）打包时注册 ufw app profile。
- [x] 权限自动化回归：jsdom 交互测试 **41 项断言**全绿（新增：协商自动推流 /
      授权弹窗允许与拒绝 / 防火墙横幅与一键放行）；Rust 单测新增 6 项
      （协商签发/信任持久化/设备身份/ufw 解析/子网推导）；check.sh full 全绿

## 阶段 C：弱网与长跑韧性（非功能指标）

> 对齐：§6.2 弱网 5–10% 丢包不冻结超过 1s、断线自动重连；内存有界。
> 参考：netem+tc 弱网仿真框架、Sunshine 自适应/FEC。

- [ ] C1 弱网测试自动化（补已知③）：`scripts/weaknet-test.sh` 用
      `tc netem` 注入丢包/延迟/抖动（5%、10%、5%+50ms 抖动三档），
      跑 push/receive 全链路，断言"冻结时长 ≤ 1s"（receive 侧按帧
      时间戳统计最大间隔）；清理规则用 trap 保证；
- [ ] C2 弱网韧性手段：
      - QUIC/SRT 重传与 ARQ 时延预算参数化（已有传输，按档位调）；
      - 关键帧自愈（已完成）在丢包下的表现验证：丢包期间花屏 →
        下一关键帧（2s 内）恢复；
      - （评估）FEC：Sunshine 路线，若 10% 丢包下恢复时长超标再引入；
- [ ] C3 长跑稳定性：30 分钟推/看不掉帧、内存有界（jitter buffer/中继
      缓冲固定容量验证——`scripts/longrun-test.sh`，RSS 上界断言）；
- 验收：C1 三档全绿；C3 30 分钟零掉帧 + RSS 平稳。

## 阶段 D：多端互通与网格完善

> 对齐：一期验收 2（手机接收电脑）、6（跨平台互通矩阵）；F5.1（PC/手机
> 同一交互）。

- [ ] D1 Android GUI：Tauri Android 复用 `apps/stross-gui/web` 网格交互
      （扫描 → 选共享 → 选接收 → 建立）；
- [ ] D2 跨平台互通矩阵实测：Linux PC ↔ Windows PC ↔ Android 任意两两
      （屏幕/声音/麦克风反向）；
- [ ] D3 传输统计推送（D5）：确认 watch channel 周期上报已进内核聚合、
      F5.3 运行页可显示；
- [ ] D4 （远期，roadmap P4）跨网段自动路由：锚点可达性探测 + 自动选
      最近可达中继建级联；mDNS 覆盖不到时 DHT/手动注册；
- [ ] D5 （远期，roadmap P3）AV Sync：统一时钟基准 + 接收端按 pts 漂移
      动态补偿；若上 RTP/WebRTC 则需 RTCP SR；
- 验收：D2 互通矩阵全通（一期验收 6）。

## 阶段 E：二期无损共享（ReliableChannel）

> 对齐：F6.1 文件互传、F6.2 剪贴板同步；需求 §4.4 无损通道设计。

- [ ] 无损通道：分块 + 滑动窗口 + 重传 + 校验（纯逻辑、可单测）；
- [ ] 文件互传 UI（网格页"发送文件"）；
- [ ] 剪贴板同步（文本起步）；
- 验收：大文件（≥100MB）局域网传输校验通过；剪贴板双向同步。

---

## 一期验收矩阵对照

| 一期验收（requirements §8） | 覆盖阶段 |
|---|---|
| 1. PC + 手机打开即互相发现，列表显示名称与能力徽标 | A2/A4、D1 |
| 2. 手机选择"接收电脑屏幕/声音"→ 确认 → 原生播放 | D1/D2（桌面侧链路已通） |
| 3. **电脑选择"接收手机麦克风"→ 确认 → 电脑播放手机声音** | **B 阶段（核心）** |
| 4. 会话运行中可停止单项共享、断开会话；断线自动重连 | B6、C2 |
| 5. 移除浏览器观看页后，控制 API/信令不受影响 | A3（docs 同步）+ 已有测试 |
| 6. 跨平台互通矩阵通过 | D2 |

## 建议推进顺序（截至第十二轮）

1. **P0-2 断线自动重连**：接收端退避重连 + 推流端重 Hello +
   拒绝原因透传——闭环验收 4 的最后缺口；
2. **C 阶段（弱网与长跑）**：`weaknet-test.sh` 10% 丢包档 SRT 调参（上调
   `rcv_latency`/ARQ 参数）+ 长跑自动化固化——弱网基线已测；
3. **D 阶段收尾**：跨平台互通矩阵（Windows/Android 实测）、传输统计推送进
   运行页（F5.3）、AV Sync（P3）；
4. **E 二期**：剪贴板同步等 ReliableChannel 剩余项（文件端点已落地）。

---


## 轮次索引（已完成轮次一句话摘要；详细记录见 git 历史）

| 轮次 | 一句话 |
|---|---|
| 第七轮 | 统一化重构：stross-core→stross-kernel 吸收 stross-app，单一 Kernel 门面 + stross-bridge 独立 |
| 第八轮 | UI 收敛「通告+订阅」+ 内核推流状态修复 |
| 第九轮 | 端点插件区 + Wayland 屏幕采集 |
| 第十轮 | 播放体验：低延迟调参 + PTS 调度层 + 延迟回归固化 |
| 第十一轮 | clippy pedantic 白名单 + 双代理审查修复 + 播放器完善 |
| 第十二轮 | 运行闭环 P0-1：端点共享生命周期治理（可停止/自动收尾/订阅收敛） |
| 第十三轮 | 实机打包 + 真机验证 + UI 优化 |
| 第十四轮 | 真机端到端闭环跑通（P0-1 生命周期） |
| 第十五轮 | mDNS 复盘 + 「可被发现」显式开关 |
| 第十六轮 | mDNS 真机「手机收不到下行多播 / PC 扫不到手机」根因定论 |
| 第十七轮 | 统一发现端口：mDNS 与子网扫描收敛到同一节点（18779 权威） |
| 第十八轮 | 基线回归：本地双端 + 真机双向闭环 |
| 第十九轮 | 交互模型定稿 + UI 术语清理 + 协商应答 panic 修复（async） |
| 第二十轮 | 发现/UI 缺陷修复（可被发现门控 + 去重）+ 死代码清理 |
| 第二十一轮 | PC UI 布局重做 + 去技术用语 + 停止共享 panic + Android 全屏修复 |
| 第二十二轮 | 推流引擎并发化（engines: HashMap）+ 离线节点剔除 |
| 第二十三轮 | 通信模式 v2 设计提案 + 文档去重/AGENTS 维护 |
| 第二十四轮 | 通信模式 v2 Phase A/B（pick 规则层：端点档案 + 解读模块）+ 订阅驱动定稿（取消 push）+ 真机回归修 Bug（watchUrls fake-IP / 确认弹窗措辞 / 设备名 placeholder） |
| 第二十五轮 | **端点模型 v2 落地**：三层统一注册表（节点→端点→策略，本机+互联节点同一张表）+ 策略组合（`EndpointStrategy`：序列化规则 + pick 规则，`strategy()` 替代 `pick_rule()`）+ 分享端/订阅端双特性（`subscribe` + 订阅端点生成 `FileReceiveEndpoint`）+ 协商/订阅按 `(节点, 端点, 策略)` 定位；v1 归档为历史指针 |

## 近期关键结论（细节以 AGENTS.md / dev-playbook.md / comm-mode-v2.md 为准）

- **术语**：共享/订阅；方向系统定；不用「通告/广播」。
- **推流并发化**：`engines: HashMap<stream_id, RunningStream>`；接收端仍单流（需 v2）。
- **通信模式 v2**：控制面协商 + 数据面按 id 复用（Phase A/B/C，允许破坏性更新）。
- **订阅驱动定稿**：数据流一律由订阅方发起并主动取（pull），共享方只在本地中继
  发布，取消 push（边主动推送）；pick 规则层 `stross-kernel::pick/`（加载/解读/
  注册表/抖动缓冲），`InterpretProfile → PickRule`。真机验证：PC 订阅手机麦克风
  走 pull（连手机中继），PC serve 无 push 流。
- **构建**：JDK 21（Gradle 8 与 25 不兼容）；前端改后需 clean 重build 嵌入；Android 构建见 android-build.md。
