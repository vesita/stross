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
| C | 弱网与长跑韧性 | §6.2 弱网 5–10% 丢包不冻结 >1s、内存有界 | 弱网测试自动化 + 韧性手段 + 长跑验证 | ⏳ 待开始（弱网基线已测，见 stress-test-report.md） |
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

## 建议推进顺序（截至第十一轮）

1. **C 阶段（弱网与长跑）**：`weaknet-test.sh` 10% 丢包档 SRT 调参（上调
   `rcv_latency`/ARQ 参数）+ 长跑自动化固化——弱网基线已测（stress-test-report.md）；
2. **D 阶段收尾**：跨平台互通矩阵（Windows/Android 实测）、传输统计推送进
   运行页（F5.3）、AV Sync（P3）；
3. **E 二期**：剪贴板同步等 ReliableChannel 剩余项（文件端点已落地）。

---

## 第七轮（统一化重构，2026-08-28）：内核定义落定 + 桥接层独立

**目标**：解决「内核定义不明确」「app 里写内核太奇怪」——`stross-core` 更名
`stross-kernel` 并吸收原 `stross-app` 全部服务；平台适应独立 `stross-bridge`；
壳层只做参数解析 + 展示 + 平台适配（docs/layering-architecture.md §2/§3）。

**变更**：
| 项 | 落点 |
|---|---|
| crate 更名 | `stross-core` → `stross-kernel`；`stross-app` 删除（服务进 kernel，平台进 bridge） |
| 单一门面 | 原 `kernel::Kernel`（会话/路由骨架）+ 原 `StrossApp`（运行态状态机）合并为 `stross_kernel::Kernel`（90+ 方法，事件经 `KernelEvent` 广播） |
| 桥接层（新 crate） | `stross-bridge`：paths（数据目录）/ hostname（OS 调用收敛）/ 平台设备枚举（`cfg(target_os)` 唯一出现点）；只产出参数注入内核 |
| 内核零 OS 调用 | `Kernel::start_relay*` / `bootstrap::ensure_identity` 主机名改为调用方注入；设备清单一律 `seed_device` 注入 |
| 服务全量进内核 | 控制面（CtrlServer+client）/ 协商（srv+client）/ 订阅 / 文件传输 / 引导 / 扫描聚合 / 推流引擎 / 接收编排 / 端点框架 / 展示视图 |
| 端口常量 | `relay::{DEFAULT_PORT=18777, GUI_PORT=8777}`、CTRL 18778、NEGOTIATOR 18779、SRT 33462、QUIC 33464（壳层/前端引用常量，无硬编码） |
| UI 收尾 | 移除「本机入口」IP 列表（mDNS 自动发现后已无用途；`app_info().ips` 保留供调试）；GUI 平台设备播种接线到 bridge（此前漏接） |

**验证**：
- `cargo check --workspace` 零错误零警告；clippy `-D warnings` 通过；前端 tsc + jsdom 通过；
- 全量单测 + 集成测试通过（含恢复的 sender_e2e / receive_roundtrip / kernel_relay_roundtrip 真实 ffmpeg 链路）；
- `share-token-test.sh` ✅（164 帧 / 277 音频块）；`latency-stability-test.sh 60 ws` ✅（1726/1800 帧、2724/2820 音频块、p99−min≈12.7ms）；
- 真机（OPPO PLC110，adb + CDP）：Android APK 构建安装成功；**互相发现**（PC 18777 ↔ 手机 8777 GUI_PORT）；**自动协商**（手机请求 → PC `negotiator-respond` 签发 grant → 手机 QUIC 推流进 PC 中继，Hello 接受、流启动）；
- 遗留（非本轮回归，属手机侧/前端既有问题，待 UI 阶段）：手机麦克风推流 10s 内无媒体帧到达（采集/QUIC 推送链，与重构无关——PC↔PC 全链路回归通过）；协商应答曾有一次挂起（瞬时锁竞争，重试即通）；设备列表 5s 轮询重建会清空「接收手机麦克风」凭证展示框；Android `hostname` 恒为 localhost（设备名显示为 localhost）。

---

## 第八轮（UI 收敛到「通告 + 订阅」+ 内核推流状态修复，2026-08-28）

**目标**：按用户意见把 UI 收敛为「本机设备通告 + 对端设备通告订阅」两极，
删除旧广播/凭证/共享流模型 UI；修复数据面流结束后引擎状态滞留导致的
「已经在推流中」卡死；消除设备名「Stross 本机中继」歧义；真机闭环验证。

**变更**：
| 项 | 落点 |
|---|---|
| 内核推流状态修复 | `Kernel.engine` 改 `Arc<Mutex<Option<RunningStream>>>`（kernel/mod.rs）；`attach_data_plane` 转发任务捕获 Arc，`RelayEvent::StreamEnded` 时若正是当前推流则 `take()` 并 `spawn(stop())` —— 采集进程中途退出后不再卡死后续端点自动推流 |
| 端点订阅闭环（上一轮续） | `subscriber::subscribe_media` 握手 → 订阅达成 → `endpoint_driver` 自动开推（真机验证：手机订阅 PC `screen:0` → PC 自动推流 sess-1 → 手机收到 102 帧） |
| UI 收敛（按用户指示） | 删除本机卡片「共享屏幕/麦克风（广播）」「接收手机麦克风」+ 凭证面板、对端卡片「共享麦克风到 TA」；删除右栏「共享流」面板 → 改为「接收」面板（订阅流播放/停止）；删除 publish.ts / negotiate.ts 两个死文件（tsconfig/index.html/测试同步）；「任何可共享设备都是节点端点，统一走通告/订阅」 |
| 前端渲染签名门控 | `refreshDevices` 设备列表签名 + `refreshLocalCatalog` 目录签名：数据未变跳过重建——**消灭 2s/5s 轮询整树重绘导致的本机设备闪烁**；`renderDeviceList` 在卡片入 DOM 后渲染设备树（修构造期容器未入文档导致的空渲） |
| 对端目录 TTL | `remoteDirs` 缓存 20s TTL（`remoteDirAt` 时间戳）：对端新通告/取消通告及时可见 |
| 设备命名 | 内核广播名从硬编码「Stross 本机中继」改为**注入的主机名**（`DiscoveryInfo::relay_default(hostname, …)`）；`bridge::device_name_or` 过滤空/localhost/android 占位主机名（Android 恒 localhost）→ 回退「Stross 设备」；GUI/CLI 壳层注入点全部切换；`view::relay_info` 增 hostname 参数保持本机视图名一致 |

**验证**：
- 内核 `cargo test -p stross-kernel` 19 项 ✅；bridge 6 项 ✅（新增占位主机名过滤测试）；workspace 全量 ✅；clippy 0 告警；fmt 干净；
- 前端 tsc ✅；jsdom 56 项断言全过（重写为通告/订阅流程：目录拉取 → 订阅握手 → 接收面板 → 断流自愈 → 遗留 UI 移除断言 → 通告/取消通告闭环 → 授权 → 防火墙）；
- 真机（OPPO PLC110，adb + CDP，**一次性干净流程**，不再乱点）：冷启动 → 新 UI（本机设备树+通告按钮、对端目录、右栏「接收」）→ 展开 PC → 目录拉到「屏幕/麦克风 公开·拉取」→ 订阅屏幕 → PC 日志 `端点 screen:0 已自动推流（Screen）: stream=sess-1 订阅方 09ac…` → 手机「接收中 · 收到 102 帧 · 已绘制 49 帧」→ 停止接收回「未接收」✅（内核修复生效：无「已经在推流中」卡死）；
- 命名验证：手机端 PC 卡片显示真实主机名 **noxy**（原「Stross 本机中继」）✅；
- 遗留：Android 设备树仅有 麦克风/系统声音（平台设备枚举无屏幕项——Android 屏幕共享走采集授权路径，非端点设备，待后续）；PC 无麦克风硬件（媒体闭环用屏幕端点验证）。

---

## 第九轮（端点插件区 + Wayland 屏幕采集，2026-08-29）

**目标**：修复 PC 端（Wayland 桌面）无法获取屏幕的 bug；按「端点插件区」愿景落地
`stross-endpoint` 取代 `stross-media`，内核收敛为纯管理调度。

**变更**：
| 项 | 落点 |
|---|---|
| Wayland 屏幕采集 | `stross-endpoint/src/screen/wayland.rs`：XDG Desktop Portal（ashpd 0.13 `select_sources` + lamco-portal 会话）→ pipewire **SHM/CPU 路径**（`dmabuf=false`，合成器无关，规避 AMD linear dmabuf mmap 全零）→ BGRA 缩放转 yuv420p → 按目标帧率节流喂 ffmpeg rawvideo stdin 编码 H.264 |
| 静止画面保活 | KWin portal 流是 damage 驱动，桌面静止即无新帧；空闲时按目标帧率**重发上一帧**：中继 `PUSH_SILENCE_TIMEOUT` 不再拆流，GOP 正常（关键帧 2s 一次，新观看端可随时接入） |
| 采集错误上抛 | 采集进程异常 / portal 错误经 `CaptureStatus.error` 上报，不再静默黑屏 |
| 端点插件区 crate | `stross-endpoint` 吸收并取代 `stross-media`：`Endpoint` 契约（端点化 + 数据还原，`EndpointApp`/`EndpointSeeder` 注入，不依赖内核）；端点 screen/（linux wayland+x11、windows、macos）、audio/、file/；采集与还原机制（capture/pipeline/playback/devices）+ codec/(nal,adts) convert/(yuv) 数据处理辅助；新增数据源 = 加目录实现契约即挂载 |
| 内核收敛 | 内核只做管理调度（端点注册表 + `impl EndpointApp`/`EndpointSeeder`），零媒体数据面细节；`Platform` 枚举移入 `stross-proto`（kernel ← endpoint 无环）；`stross_kernel::ScreenEndpoint` 等路径重导出保持壳层兼容 |
| 依赖统一 | 新增 ashpd 0.13 / lamco-pipewire 0.6.10 / lamco-portal 0.4.4，全部进根 `[workspace.dependencies]` |

**验证**：
- `cargo build --workspace`（default 与 `--no-default-features`）零警告；clippy `-D warnings`、fmt ✅；前端 tsc + jsdom ✅；
- 全量单测 ✅（endpoint 47 项含真实 ffmpeg 链路；kernel/bridge/proto/types/transport 全部通过）；
- 实机 Wayland e2e（KDE Plasma + AMD）：`stross push --screen` + `stross receive` 录制 **212 帧 1280×720、185 个色值分布的真实桌面内容**，流全程存活（保活修复生效）✅；
- 提交 `43b8661`（63 文件，+2323/−918）。

**遗留 / 待定**：
- [ ] **PTS 驱动的播放调度层（待用户拍板）**：播放侧当前无调度层——帧进即解即出，延迟随缘。方案：抖动缓冲 + 目标延迟 ~150-200ms + 本地播放时钟按 `pts` 相对间距调度；过水位视频丢帧追平到实时（复用 `try_send` 丢帧语义，改按 pts 判定）、欠水位补帧/插静音、大 PTS 跳变重置缓冲。视频不采用倍速追平（WebRTC 同款结论）；倍速仅留给音频 NetEq 式时间伸缩（LAN 场景暂不需要）。
- [ ] flake 修复备注：`decoded_pixels_match_native_ffmpeg` 根因是测试「先推完再读」= 推流期零消费，32 槽有界通道被解码瞬时超前顶满 → 修复为推流期并发排空（等价真实渲染循环）；产品侧丢帧语义不变（第九轮提交后单独修复，待提交）。

---

## 第十轮（播放体验：低延迟调参 + PTS 调度层 + 延迟回归固化，2026-08-29）

**目标**（用户拍板：播放体验/延迟优先）：P0 SRT 低延迟调参 → P1 PTS 驱动播放
调度层 → P2 延迟/节奏回归固化（验证：全量测试 + 实测）。

**变更**：
| 项 | 落点 |
|---|---|
| SRT 低延迟调参（P0） | `stross-transport/src/srt.rs`：`DEFAULT_SRT_LATENCY_MS` 120→**20**（40 实测 min 149-181ms 随负载漂移，20 稳定 ~143ms）；`srt_options()` 统一 bind/connect 两端；`STROSS_SRT_LATENCY_MS` 环境覆盖 + 回退单测 |
| PTS 驱动播放调度层（P1） | `stross-endpoint/src/playback/schedule.rs`（新建）：`PlaybackScheduler` 纯逻辑 + 时间注入；锚定 `play(pts) = anchor + (pts−pts0)`；过水位按**队尾判据**丢最新帧钳制显示延迟；PTS 大跳变重置重锚；迟到帧立即发。`VideoPacing{target_delay:150ms, jump_reset:500ms}`，仅实时显示路径启用（headless/录制直通）；pacer 线程 std `sync_channel` + `recv_timeout` |
| **ffmpeg 解码帧线程延迟（本轮回溯根因，-threads 1）** | `playback/ffmpeg.rs` 视频解码参数新增 `-threads 1`：h264 解码器默认帧线程数 = CPU 核数（本机 16），输出被管线延迟 (threads−1) 帧 —— **绝对延迟 566ms 的元凶**（隔离复现：30fps 喂流首帧延迟 = 16×33ms≈530ms + 解析 ~40ms）。单线程 720p30 解码余量充足，低延迟优先 |
| 绝对延迟测量口径修正 | `latency-stability-test.sh`：期望帧/音频块按接收接入时移（~2s）折算（拓扑时移 ≠ 丢帧，10s 轮实测 241/300 帧 = 8.0s×30 ✓）；`MAX_ABS_MIN` 重校为**含解码管线**口径（B4 QUIC min≈0.9ms 是不含解码的传输层口径）：WS/SRT ≤200ms、QUIC ≤120ms，仅作回归保护 |

**验证**：
- `latency-stability-test.sh 10 ws srt quic` **三传输全达标**：WS min=99.9 / SRT min=145.4 / QUIC min=100.4 ms（修复前三传输均 ~566ms，-5.6x）；60s 长跑 srt/quic 亦达标（SRT min=171.3 为并发测试负载下，p99−min=71ms ≤250 尾延迟上界）；
- workspace 全量测试 ✅（endpoint 58 / kernel 85 / transport 20 / 其余全绿）；clippy 0 告警；fmt 干净；前端 tsc + jsdom ✅；
- 探针回放实证（隔离 + 真机同参数）：raw h264 demuxer 管道输入首帧延迟与解码帧线程数严格线性（threads N → 延迟 (N−1)×帧间隔），`-threads 1` 后 30fps 喂流首帧 ~73ms；
- 中继补发语义确认：观看端接入 = 最近关键帧 + **实时帧**（非历史突发重放），无 burst 污染。

**遗留 / 待定**：
- [ ] 真机（手机→PC / PC→手机）延迟复测：本机双开已达标，跨设备路径（弱网/多跳）待用户环境实测；
- [ ] QUIC ≤30ms 的传输层口径如需保留，需把延迟测量拆成「传输层」（不含解码）与「端到端含解码」两个指标；
- [ ] 调度层（P1）的播放节奏在 GUI 实时显示路径的实测节奏回归（headless 直通不经过 pacer，本轮未覆盖 GUI 侧节奏）。

## 第十一轮（代码质量提升：clippy pedantic 白名单 + 双代理审查修复 + 播放器完善，2026-08-29）

**目标**：系统性代码质量提升。子代理双线审查（Rust 核心 77 文件 / GUI+前端 18 文件）
+ clippy pedantic/nursery 统计（~1200 告警，doc 类噪音为主），实施高价值低风险改进。

**变更**：
| 项 | 落点 |
|---|---|
| clippy 白名单机械修复（68 文件） | 仅启用代码类 lint：`redundant_closure` / `uninlined_format_args` / `manual_let_else` / `single_match` / `map_unwrap_or(_else)` / `use_self` / `missing_const_for_fn` / `redundant_field_names` / `manual_midpoint`（adb bounds 中心点 u32 溢出 → `u32::midpoint`）/ `semicolon_if_nothing_returned` / `suboptimal_flops` / `cast_lossless`。**排除 doc_markdown 等文档类**（--fix 自动插 backtick 会切碎中文文档，曾误改后全量回退重做） |
| **SRT 分片计数溢出（高）** | `srt.rs`：frag_cnt/frag_idx 为 u8，载荷 >255×1400B（4K IDR 可达）时回绕 → 接收端把消息当整帧逐片吐出（花屏）。发送前显式拒绝超限载荷 + 单测 `srt_rejects_fragment_overflow` |
| **QUIC 读侧 4GiB OOM（高）** | `quic.rs`：读侧长度由对端声明无上限，任意可完成 TLS 的对端发 `0xFFFFFFFF` 即触发 4GiB 分配（Android 中继 OOM）。加 `MAX_MSG_LEN = 64MB` 与写侧对齐 |
| **pacer wait==0 阻塞（高）** | `ffmpeg.rs::pacer_loop`：队首恰到期时 `recv_timeout` 超时后下一轮 wait==0 落入**阻塞 `recv()`**，被 hold 的帧推迟到下一输入帧才发出（突发流批量倾泻）。修复：循环顶部先 `emit_due`，wait==0 用 `recv_timeout(ZERO)` 空转补发 |
| **pts 队列代际不清空（中）** | `ffmpeg.rs` 失步重建路径清空 `shared.pts`：旧代际被管道吞入未产出的帧对应 pts 残留，新代际前 N 帧弹到过期 pts → 时间戳回退/跳变 |
| **subscriber 网络输入 unreachable!（中）** | `subscriber.rs` 两处 `Delivery::Both => unreachable!`（对端版本偏差可触发）→ `anyhow::bail` |
| **bgra_to_yuv420p_scaled 文档与实现不符（中）** | `convert/yuv.rs`：文档声称双线性、实现是 floor 最近邻（屏幕文本缩放下锯齿）。补齐真双线性（中心对齐 + clamp，与 `rgba::rgba_scaled` 同约定） |
| **中继锁统一 lock_poisoned（中）** | `relay/state.rs` 12 处 `.lock().unwrap()` → `lock_poisoned()`（本 crate lock.rs 自建约定，唯一违规点） |
| **前端错误显示全失效（高）** | `ui.ts` 新增 `errMsg(e)`：Tauri 命令失败 rejection 是**字符串**非 Error 对象，`(e as Error).message` 显示 undefined。替换全部 10 处调用点 |
| **startReceive 防重入 + 失败回滚（高）** | `subscribe.ts`：入口先 `stopReceive()` 清理旧会话；Rust 会话启动后接线失败 → `stop_receive` 回滚（消除收流/发声却无法停止的泄漏会话 + 监听器泄漏 + 轮询双链） |
| **轮询边界修复（中）** | `pollReceiveStatus`：`!running` 即收尾（原要求 received>0，未接通时永久卡「等待流数据」）；await 后复查 receiving 防过期写 DOM；`pollMicRecv` 加到期/60 次轮次上限（凭证永不兑现时 2s 永久轮询泄漏） |
| **drawReceiveFrame 热路径复用（中）** | `ui.ts`：缓存 RGBA 缓冲 + ImageData（构造引用不拷贝），尺寸不变时零重复分配；长度不符帧防御（防 ImageData 构造抛错） |
| **死代码清理（低）** | `state.ts` 9 个无消费者变量（publishing/shareKind/micShare/localStreams 等遗留）；`ui.ts` showView/renderUrls/urlListItem/fmtElapsed；`commands.rs` 死语句 `let (_, _) = required_firewall_ports(...)` |
| **前端测试 [5] mock 替换无效修复** | `test-frontend.mjs`：ui.ts 的 `call()` 在 eval 时捕获 invoke 引用，属性替换无效 → 改 mock 内可变开关 `scanReturnOverride`，[5] 场景真实走空列表路径 |
| **video_pacing 测试 flake 根治** | `ffmpeg.rs` 测试：`capture_frames` 返回 1s@30fps ≈30 帧，全部重写 pts 后第 17 帧 528ms 越过 500ms 跳变阈值触发重锚 → span 断言失真。修复：`frames.truncate(5)` 只验证 0..132ms 五帧；节奏断言收紧为「有帧被 hold 才断言 span」（held>0 时 span≥33ms 必然） |

**验证**：
- `scripts/check.sh` full 全绿（fmt / clippy -D warnings / workspace 测试 / tsc / js 同步 / jsdom 35 断言）；
- endpoint 61 / kernel 85 / transport 21（+SRT 上限单测）全过；video_pacing 连续 10+ 次稳定；
- clippy pedantic/nursery 剩余告警全部为**文档类与噪音类**（doc_markdown 中文误报、must_use_candidate、needless_pass_by_value 的 Tauri command 签名约束），未启用为门禁。

**遗留 / 待定**（审查报告其余项，收益/风险权衡后未做）：
- [ ] `sender.rs` Hello 错误传播（推流拒绝时 connect 假成功）——状态机改动面大，留待推流 UX 专项；
- [ ] `webrtc.rs` inbound `send().await` 反压阻塞 run loop → 改 `try_send` + 丢帧计数（媒体通道语义对齐）；
- [ ] mDNS daemon `OnceLock<Result<…>>` 收敛（kernel discovery + webrtc 两处 expect panic）—— 待确认 ServiceDaemon 失败面；
- [ ] 凭证校验实现两遍（`verify_share_token` vs `KernelTokenValidator::validate`）收敛；
- [ ] 中继/协商 CORS 中间件两份收敛；URL host:port 解析三份收敛（统一 `RelayUrl`）；
- [ ] discovery 未知枚举值整设备消失（容忍反序列化）——混合版本演进策略需单独评估；
- [ ] `frame.rs` 32 位目标 len 回绕 panic（`checked_add` 一行）；`pts_ms: u32` 回绕（49.7 天会话上限声明）；
- [ ] 全量 must_use_candidate（186 处）与 # Errors/# Panics 文档补齐（量大，可分批）。
