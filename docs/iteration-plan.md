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

## 建议推进顺序（截至第十二轮）

1. **P0-2 断线自动重连**（closed-loop-plan.md §3）：接收端退避重连 + 推流端重 Hello +
   拒绝原因透传——闭环验收 4 的最后缺口；
2. **C 阶段（弱网与长跑）**：`weaknet-test.sh` 10% 丢包档 SRT 调参（上调
   `rcv_latency`/ARQ 参数）+ 长跑自动化固化——弱网基线已测（stress-test-report.md）；
3. **D 阶段收尾**：跨平台互通矩阵（Windows/Android 实测）、传输统计推送进
   运行页（F5.3）、AV Sync（P3）；
4. **E 二期**：剪贴板同步等 ReliableChannel 剩余项（文件端点已落地）。

---

## 第七轮（统一化重构，2026-08-28）：内核定义落定 + 桥接层独立

**目标**：解决「内核定义不明确」「app 里写内核太奇怪」——`stross-core` 更名
`stross-kernel` 并吸收原 `stross-app` 全部服务；平台适应独立 `stross-bridge`；
壳层只做参数解析 + 展示 + 平台适配。**产出**：单一 `Kernel` 门面（90+ 方法，事件经
`KernelEvent` 广播）；端口常量收敛（relay 18777 / GUI 8777 / ctrl 18778 / 协商 18779 /
SRT 33462 / QUIC 33464）；目录与订阅握手收敛。**验证**：workspace 零警告；真机互相发现 +
自动协商闭环（详细变更见 git 历史）。

## 第八轮（UI 收敛到「通告 + 订阅」+ 内核推流状态修复，2026-08-28）

**目标**：按用户意见把 UI 收敛为「本机设备通告 + 对端设备通告订阅」两极，删除旧
广播/凭证/共享流面板（publish.ts / negotiate.ts 删除，右栏改「接收」面板）；修复数据面
流结束后引擎状态滞留导致的「已经在推流中」卡死。**产出**：`Kernel.engine` 改
`Arc<Mutex<Option<RunningStream>>>` + `StreamEnded` 自动清理；端点订阅闭环（订阅达成 →
端点自动开推）；前端渲染签名门控 + 对端目录 20s TTL；设备命名用注入主机名（Android
回退「Stross 设备」）。**验证**：真机「手机订阅 PC 屏幕 → 自动推流 → 手机收帧」闭环；
前端 jsdom 56 项断言。

## 第九轮（端点插件区 + Wayland 屏幕采集，2026-08-29）

**目标**：修复 PC 端（Wayland）无法获取屏幕的 bug；按「端点插件区」愿景落地
`stross-endpoint` 取代 `stross-media`，内核收敛为纯管理调度。**产出**：Wayland 屏幕采集
（portal+pipewire SHM 路径）+ 静止画面保活（按目标帧率重发上一帧）；`Endpoint` 契约
（load/share，端点自驱动）+ screen/audio/file 端点 + codec/convert 处理辅助；
`Platform` 移入 stross-proto。**验证**：Wayland 实机 e2e 212 帧真实桌面内容；提交 `43b8661`。

## 第十轮（播放体验：低延迟调参 + PTS 调度层 + 延迟回归固化，2026-08-29）

**目标**（用户拍板）：播放体验/延迟优先——SRT 低延迟调参 → PTS 驱动播放调度层 →
延迟/节奏回归固化。**产出**：`DEFAULT_SRT_LATENCY_MS` 120→20（实测 ~143ms 稳定）；
`PlaybackScheduler`（`VideoPacing{target_delay:150ms, jump_reset:500ms}`，仅实时显示路径）；
ffmpeg 解码 `-threads 1`（**绝对延迟 566ms 元凶**，-5.6x）；延迟测量口径修正
（含解码管线）。**验证**：三传输全达标（WS min=99.9 / SRT min=145.4 / QUIC min=100.4 ms）。

## 第十一轮（代码质量提升：clippy pedantic 白名单 + 双代理审查修复 + 播放器完善，2026-08-29）

**目标**：系统性代码质量提升——clippy 代码类 lint 白名单机械修复（68 文件）+ 双代理
审查驱动高危 Bug 修复。**产出**（高价值项）：SRT 分片计数 u8 回绕拒绝、QUIC 读侧
4GiB OOM 上限（`MAX_MSG_LEN=64MB`）、pacer wait==0 阻塞、pts 队列代际清空、
subscriber `Delivery::Both` unreachable→bail、前端错误显示全失效（errMsg）、
startReceive 防重入+失败回滚、轮询边界修复、drawReceiveFrame 热路径复用。
**验证**：`check.sh` full 全绿；kernel 85 / endpoint 61 / transport 21 全过。

### 历史遗留（第七~十一轮仍有效，去重合并；其余细节见 git 历史）

- [ ] Android 设备树无屏幕端点（Android 屏幕共享走采集授权路径，非端点模型，待后续）；
- [ ] 真机（手机→PC / PC→手机）延迟复测待用户环境实测（本机双开已达标）；
- [ ] 延迟测量拆「传输层 / 端到端含解码」两指标（如需保留 QUIC ≤30ms 口径）；
- [ ] PTS 调度层（P1）在 GUI 实时显示路径的节奏回归（headless 直通不经过 pacer）；
- [ ] `sender.rs` Hello 错误传播（推流拒绝时 connect 假成功）——留待 P0-2 推流 UX 专项；
- [ ] `webrtc.rs` inbound `send().await` 反压阻塞 run loop → `try_send` + 丢帧计数；
- [ ] mDNS daemon `OnceLock<Result<…>>` 收敛（两处 expect panic）；
- [ ] 凭证校验实现两遍（`verify_share_token` vs `KernelTokenValidator`）收敛；
- [ ] 中继/协商 CORS 中间件两份收敛；URL host:port 解析三份收敛（统一 `RelayUrl`）；
- [ ] discovery 未知枚举值整设备消失（容忍反序列化）——混合版本演进策略需单独评估；
- [ ] `frame.rs` 32 位目标 len 回绕 panic（`checked_add` 一行）；`pts_ms: u32` 回绕（49.7 天会话上限声明）；
- [ ] 全量 must_use_candidate（186 处）与 # Errors/# Panics 文档补齐（量大，可分批）。

---

## 第十二轮（运行闭环 P0-1：端点共享生命周期治理，2026-09）

> 方案：`docs/closed-loop-plan.md`。范围声明：开发期不做 wire 版本兼容；
> 本轮只做工程闭环，协议优化（watch 鉴权 / 保活控制帧等）排后续。

**目标**（对应闭环缺陷 #5/#6/#8/#9）：端点共享可停止、订阅者全断开自动收尾、
同端点多订阅者收敛（pull 复用流）、会话/凭证/授权随流结束清理、端点状态实时更新。

**变更**：
| 项 | 落点 |
|---|---|
| 端点共享登记 | `EndpointApp::note_share_active(weak, endpoint_id, stream_id, delivery)` 新增（默认空实现）；`spawn_media_share` 传 endpoint_id（screen/audio 三处调用点）；Kernel `active_shares` 表（stream_id → 端点）+ `set_state(Active)` + 无观看者接入窗口兜底（`share_idle_delay` 10s，经 Weak 回调不拖住内核） |
| 停止路径 | `Kernel::stop_endpoint_share`（幂等）+ `stop_share_by_stream`（同步：取引擎 spawn 收尾 + teardown + 清登记 + Idle）；`unpublish_endpoint` 改 async 并**联动停止活动共享**；GUI 新命令 `endpoint_stop_share` + 本机端点树「停止共享」按钮（state=active 时显示，badge live 样式） |
| watchers→0 自动收尾 | `attach_data_plane`（改 `self: &Arc<Self>`）事件转发扩展：`WatchersChanged{0}` → 延迟复查（`share_stop_delay` 4s，经 `DataPlaneBackend::stream_watchers` 新增查询）仍无人观看才停；`StreamEnded` → 清登记 + Idle + **本机会话 teardown**（会话生命周期 = 流生命周期，顺带修复会话/授权/token 只增不减）；`teardown` 补充移除签发表（凭证随会话失效） |
| 订阅收敛 | `compose_grant` 先查 `active_share_by_endpoint`：pull 复用同一流（grant 只带 stream_id，不新建会话/凭证）；push 第二订阅者拒绝（清晰报错，不再"grant 成功但流不存在"）；`notify_subscribed` 复用场景不重复触发 share |
| 端点状态实时化 | `set_state` 生产接线：登记 Active / 停止 Idle / `WatchersChanged` 同步 subscribers=watchers |

**验证**：
- `scripts/check.sh` full 全绿（fmt / clippy -D warnings / workspace 测试 / tsc / js 同步 / jsdom 新增 [12] 停止共享断言）；
- kernel 单测 89 项 ✅（新增：active_share 登记/查询/停止幂等、teardown 清 token、pull 复用同流、push 第二订阅拒绝）；
- 集成测试 ✅ `endpoint_share_stops_after_last_watcher_leaves`：真实受控中继 + 双端，观看端断开 → watchers→0 延迟复查 → 登记清除 + 会话拆除 + 流回收；
- 测试暴露机制细节：中继观看端只在「下一次广播」时发现对端已断（send 失败），断连检测依赖推流端持续发帧——真实端点共享恒 30fps 推流，生产路径成立。

**遗留 / 待定**：
- [ ] P0-2 断线自动重连（接收端退避重连 + 推流端重 Hello + 拒绝原因透传），见 closed-loop-plan.md §3；
- [ ] 复用以登记为准（登记随停止/流结束同步清除）：停止与新订阅接入之间的极端竞态窗口由 P0-2 重试兜底；
- [ ] 并发双订阅同端点（同一毫秒内）仍可能双建会话（share 先到者胜）——开发期可接受；
- [ ] 协商层复用/拒绝的 HTTP 状态码暂用 500 承载业务错误（消息可读），协议优化阶段改 409；
- [ ] 协议优化阶段：watch 鉴权 + stream_id 不可枚举、应用层保活控制帧、pts 回绕（closed-loop-plan.md §5）。


---

## 第十三轮（实机打包 + 真机验证 + UI 优化，2026-09）

**起因**：用户「打包新版本上手机实机验证 P0-1 闭环 + 随手优化 UI」。

**Android 打包修复**（此前 Android target 无法编译，三处 cfg 错误）：
| 问题 | 修复 |
|---|---|
| `factory.rs` Android 分支误引用不存在的 `screen::linux::audio_probe`（Android 下该模块被 `cfg(not(target_os="linux"))` 掉） | 新增 `android_audio_probe()`（恒可用的 `Probe`：Android 音频采集走原生 MediaRecorder/AAudio，**不依赖 ffmpeg**）；desktop 分支保持 `screen::*::audio_probe` |
| `lib.rs` 无条件 re-export `playback::FfmpegPlaybackSink`（Android 下被 `cfg(not(target_os="android"))` gate 掉） | 该 re-export 单独 `#[cfg(not(target_os="android"))]` |
| 中途曾把 `audio::audio_probe`（ffmpeg 检查）用作 Android 探测，导致真机端点「不可用（ffmpeg 不可用）」 | 撤回（改恒可用 probe），避免 dead_code |

**打包链环境**：项目 `build.gradle.kts` 钉 `compileSdk=36/buildToolsVersion=36.0.0`，但当前 `/opt/android-sdk` 仅装 build-tools 37 / platform android-37，且 SDK 目录 root 所有、不可写、license 未接受。AGP 8.11 对 compileSdk 37 仅警告（非阻断），但切 37 触发的其它组件（build-tools 35）仍因 license/写权限失败。**经用户 sudo 安装** build-tools 35/36 + platform android-36 + 接受 license 后构建成功（`cargo tauri android build --debug`，JDK 17）。

**真机结果**（OPPO PLC110，Android 16，WiFi 192.168.11.60，WebView CDP 驱动）：
- debug APK 构建成功并安装（旧包为 release 签名不一致 → 卸载重装）；App 启动中继 8777 在线、WebView 远程调试通道可用；
- 真机 UI 冒烟 ✓：端点可用、通告弹窗（可见性/delivery 选择）、通告后徽标「已通告·公开·拉取」+「取消通告」均工作；
- **跨设备订阅受限**（非 P0-1 bug，实测发现）：手机监听端口**无 18779 协商服务**（仅 8777），PC `stross devices` 扫描**发现 0 台**（mDNS 隔离/多网卡）→「手机作公开方→PC 协商订阅」在当前网络/代码下不可行；P0-1 核心收尾已由集成测试覆盖。

**UI 优化**（基于真机截图，修实机抓到的缺陷）：
- **端点行竖排 bug**（P0-1 新增「已通告徽标 + 取消通告按钮」与文本同排，窄屏下名称/meta 被挤压成逐字竖排）→ `.ep-row` 允许 `flex-wrap` + `.ep-name/.ep-meta` `white-space:nowrap` + ellipsis；
- **meta 类别去重**（名称「系统声音」+ meta「系统声」重复）→ 可用端点 meta 显示「实时」，不可用显示原因；
- **`.ep-actions` 右对齐操作组**：徽标 + 通告/取消通告按钮包进右对齐容器，避免「取消通告」按钮孤行，紧凑美观；
- jsdom [11]/[12] 回归保持全绿。

**用户诉求**：手机端点太少（Android 仅麦克风/系统声音两个音频端点）——属 P1 明确范围（屏幕需 MediaProjection 前台服务、摄像头 CameraX，均后置，见 factory.rs 注释）。用户拍板「先完成 P0-1 + UI 优化」，扩充端点记入后续。

**遗留 / 待定**（新增）：
- [ ] **扩充 Android 端点集**：屏幕（MediaProjection 前台服务采集 + Rust 端点 + 授权）、摄像头（Camera2/CameraX + 文件/剪贴板等）；Android GUI 缺协商服务（18779）→ 手机无法被自动协商订阅，需评估是否补；
- [ ] P0-2 断线自动重连（承接第十二轮，见 closed-loop-plan.md §3）；
- [ ] 跨设备端到端真机回归：当前 mDNS 隔离 + 手机无协商服务，需网络/配置支持才能跑通「通告→订阅→断开→收尾」；
- [ ] 协议优化阶段：watch 鉴权 + stream_id 不可枚举、应用层保活控制帧、pts 回绕（closed-loop-plan.md §5）。

---

## 第十四轮（真机端到端闭环跑通：P0-1 生命周期，2026-09）

**目标**：跨设备真机跑通「端点通告→订阅→断开→自动收尾」，解决上轮发现的跨设备阻塞。

**两大堵点与修复**：
| 堵点 | 根因 | 修复 |
|---|---|---|
| 手机无 18779 协商服务 | `lib.rs` 刻意「Android 仅作客户端不启动协商服务」（`#[cfg(mobile)]` 只 manage 空 handle） | 解除限制：`start_handshake` 所有平台启动；`NegotiatorUiBridge` 去掉 `#[cfg(not(mobile))]` gate（Android 前端同样订阅 `negotiator-request`）。手机 App 前台时监听 0.0.0.0:18779 |
| 无法订阅实时媒体端点 | CLI `endpoint subscribe` 只走 `subscribe_file`（文件落盘），对实时「系统声音」报「实收 0 字节」 | 新增库接口 `subscribe_media_and_watch`（`subscribe_media` 握手 → `connect_watch` 连对端中继建 watcher → 读帧保持）；CLI 按端点 kind 分派：File→落盘，其余→媒体观看保持（Ctrl-C 断开触发收尾）；导出到 `stross_kernel`。对「流尚未出现」做 `STREAM_APPEAR_WINDOW`(9s) 重试（watcher 接入与公开方泵建流竞态） |

**真机结果**（OPPO PLC110，Android 16，PC+手机同网段 192.168.11.x）：
- 手机通告麦克风（public/pull）→ PC `stross endpoint subscribe --host <手机> --endpoint mic:builtin --delivery pull`；
- **订阅达成**：手机 UI「已通告 · 公开 · 拉取 · 1 订阅中」+「停止共享」（active）；手机中继 `/api/streams` 出现 `sess-3「麦克风」watchers=1`；
- **断开自动收尾**：终止 PC 订阅进程 → 手机 watchers→0 延迟复查（4s）→ UI 回「已通告 · 公开 · 拉取」（无订阅中/停止共享）+ `/api/streams` 清空 → **P0-1 闭环真机验证成功**。

**关键发现（实测）**：
- **Android 前台约束**：手机 Stross 必须在**前台**协商服务才响应（App 后台被冻结，18779 虽监听但拉目录超时）；切前台即恢复。真机闭环需保持 Stross 前台。
- 系统声音端点 share 依赖 MediaProjection（录屏授权）较繁琐；**麦克风**（RECORD_AUDIO）更易跑通——本次用麦克风验证。
- mDNS 扫描仍 0 台（PC 侧 Mihomo TUN/fake-IP 疑似干扰组播），但**直连协商（不经 mDNS）已足够跑通闭环**——设备发现可后续借手动添加/修复 mDNS browse 接口。

**遗留 / 待定**（新增）：
- [ ] mDNS 跨设备发现修复（PC 扫 0 台；疑 Mihomo TUN 干扰组播；或用手动添加兜底已验证）——接入「设备卡」自动发现仍依赖它；
- [ ] 系统声音端点真机验证（需 MediaProjection 录屏授权 + 前台）；Android 后台保持协商服务的可行性评估（FGS）；
- [ ] P0-2 断线自动重连（承接前轮）；协议优化阶段（watch 鉴权/保活/pts 回绕）。

---

## 第十五轮（mDNS 复盘 + 「可被发现」显式开关，2026-09）

**起因**：第十四轮真机闭环虽已跑通，但 PC `stross devices` 扫仍 0 台（mDNS 自动发现失败）。复盘发现前后共修了两层，且最初的方向判断有误；最终把「发现」改为**用户显式开关**。

**对既有 mDNS 修复思路的复盘**（用户要求审阅）：
- **已做**：fork `mdns/src/service_daemon.rs` 的 `process_packet`——把「query-with-answer（手机周期公告）」误当查询忽略 answer 的分支，改为也走 `handle_response` 喂缓存。**方向对但分析片面**。
- **走偏处**：以为是「浏览端 resolve 不补全」，反复调 4s/12s 窗口。实测发现**根子在手机端**：
  1. `Discovery::start`（mDNS 注册）只在锚定中继时调（`relay/server.rs`、`kernel/mod.rs`），**端点通告/取消通告不触发重注册** → TXT `ep.*.published` 停在锚定时刻旧值（「手机广播不更新」，与用户判断一致）；
  2. 手机 App 经多次 install/`am start` 后 mDNS 状态退化（只发 PTR、不响应 SRV/TXT/A 补查）——**重启 App 重新锚定后 avahi 即能看到完整记录**（TXT 全端点、地址/端口齐全），证明手机端本可发完整记录，只是当时状态异常。
- **回退**：`process_packet` 的 patch 因破坏 mdns-sd 上游 3 个集成测试（`known-answer-suppression` 等依赖 query-with-answer 走查询语义）而**回退**——它并非正确修复，反而污染了标准的已知应答抑制。

**本轮改动：可被发现 = 用户显式开关（默认关）**
- **内核**：`Kernel` 增 `discoverable`（`AtomicBool`，默认关）；`set_discoverable`/`discoverable`；锚定中继时**仅当开启才广播**；新增 `Discovery::redefine`——**保持同 fullname 覆盖重注册**（mdns-sd `my_services` 按全名小写 `HashMap::insert`，TXT 更新即生效，避免先注销再注册空窗）。
- **立即刷新**：`publish_endpoint` / `unpublish_endpoint` / `publish_file_endpoint` 在 registry 锁**外**调 `apply_discoverable()`，通告状态一变化就重注册刷新 TXT `published`。
- **持久化**：`Settings`（`settings.json` 与 identity 同目录，`discoverable` 默认 false）；内核 `load_settings`/`save_settings`。
- **CLI**：`serve --discoverable`（显式开启；不传读 settings.json）。`relay` 独立进程维持各自 `--no-advertise`（不同制品，不并入）。
- **GUI**：header 右上「可被发现」开关；`set_discoverable`/`discoverable_status` 命令；setup 读 settings 注入。

**自测验证（`devices` 跨进程扫描）**：
- `--discoverable` 起 serve → 独立 `devices` 扫到该服务（设备名 pico）；
- 不传 `--discoverable` → 扫 0 台（默认不可被发现 ✓）；
- 通告 `screen:0` 后重扫 → TXT 显示「屏幕（已通告）」→ **通告即刷新生效** ✓；
- settings.json `discoverable:true` 不传 flag → 仍广播（持久化✓）。
- 静态门禁：`cargo build --workspace`、`cargo test -p stross-kernel`（settings/discovery/全部 93+ 用例）、clippy、fmt 全绿；前端 `tsc` 通过、`app/*.js` 再生成。

**遗留 / 待定**：
- [ ] Android GUI 端点通告 → 手机 mDNS TXT `published` 即时刷新，需真机复核（本轮只验了 CLI 路径）；
- [ ] 手动添加设备（`devices` 已有 `--extra`？/GUI manual-addr）作为 mDNS 兜底，跨设备发现仍依赖网络组播健康；
- [ ] 手机端手机 App 状态退化（只发 PTR 不响应查询）的**根因**仍需追（疑 App 重启后 daemon 未正确重建）；与本轮开关配套后，建议真机「重启 App 重新锚定」再验 mDNS。

---

## 第十六轮（mDNS 真机「手机收不到下行多播 / PC 扫不到手机」根因定论，2026-09）

**起因**：真机上 PC 用 `stross devices` 扫不到手机（手机能发现 / 曾发现电脑）。经多轮（地址匹配→探测闸门→发送接口→接口归属→TUN→网络加速）逐一排除后，最终定论在**环境侧（WiFi AP 下行多播被拦）**，而非 Stross 代码。

**排除链（每步都有日志/抓包证据）**：
1. ~~地址匹配失败~~：诊断证明 `wlan0` 的 `intf_addrs=["192.168.11.60"]` 非空，`prepare_announce` 能构造 `answers_count=4`。
2. ~~探测闸门 `probing_count>0`~~：`set_requires_probe(false)` 生效，不再 reject 整个 QR=1。
3. ~~发送接口/接口归属/`handle_read` 家族检查丢弃~~：加全量入包诊断（`recvPkt`）后，PC 的单播能到手机（`from=192.168.11.61:48779 if_index=40 intf=wlan0`），但多播从未到达手机 socket。
4. ~~PC 的 TUN / 网络加速开关~~：TUN 关闭、网络加速/双通道关闭后仍同症状；PC 出网多播计数正常（enp6s0 TX +24）。

**定论**：**手机能发上行多播、能收单播、能收自己 socket 的回环多播，唯独收不到「从 AP 下来的多播」**（`Su` 路由下行多播被 IGMP snooping/客户端隔离拦截）。同一套代码在**多播通畅的网络**（手机↔PC 经 USB 共享，同网段 10.159.157.0/24）上**双向发现均成功**：PC 扫到手机 `10.159.157.104:8777`（完整 SRV+TXT+A/端点/SRT/QUIC），手机列表出现 PC `10.159.157.158:18777`。→ **mDNS 代码无 bug，纯环境问题。**

**处理 / 结论**：
- **排查诊断日志已全部清理**（`crates/mdns/src/service_daemon.rs`、`crates/stross-kernel/src/discovery.rs` 的 `DIAG` 移除），恢复干净 `trace!`/`debug!`；`cargo test -p mdns` 全绿。
- mdns fork 保留的**功能性改动**：`set_requires_probe(false)`、`handle_read` 接口 fallback、`ingest_records`（query-with-answer 入库）、`apply_multicast_rate_limit`。
- **解决办法（任选）**：①改 `Su` 路由器（关 IGMP snooping / 关客户端隔离 / 开多播转发）；②用多播通畅网络（如 USB 共享）跑真机闭环；③可选代码兜底（mDNS 加单播下发/单播应答），让手机在收不到下行多播时仍可被发现/发现对端。

**遗留 / 待定**：
- [ ] P0-1 真机闭环走 USB 网络已可发现；如需 WiFi 也稳定，实施「mDNS 单播兜底」或修复路由器多播；
- [ ] 手机 App mDNS「只发 PTR 不响应补查」（状态退化）隐患仍存，建议配合重启 App 重新锚定核验（本轮已另作记录）。

---

## 第十七轮（统一发现端口：mDNS 与子网扫描收敛到同一节点，2026-09）

**起因**：延续第十六轮定论——`Su` 路由只拦「下行多播」、**单播双向通**（§8.2）。用户采纳「双发现机制应指向**同一节点**（降低认知成本）」的建议，选定：**协商/发现端口 18779（桌面与 Android GUI 一致）作发现权威**，mDNS 与子网扫描都据此收敛到**同一台设备同一个 `relay_port`**；保留 mDNS 为组播通畅时的首选。

**改动**：
- **`crates/stross-kernel/src/devices.rs`**：新增 `DiscoveryResp`（权威发现清单：deviceId/name/relayPort/srtPort/quicPort/roles/media/transports/endpoints）+ 发现权威端口常量 `DISCOVERY_PORT=18779`；`subnet_scan`/`scan_probe_host` 改为**单一探 `18779/api/discovery`**，以其 `relay_port` 作设备节点（不再是硬编码 `[18777,8777]`，故能发现自定义中继端口设备且与 mDNS 同节点）。
- **`crates/stross-kernel/src/kernel/mod.rs`**：新增 `discovery_manifest()`，从 `relay_ports()`+`device_identity()`+`mdns_info()` 组装清单。
- **`crates/stross-kernel/src/negotiator/*`**：18779 新增 `GET /api/discovery`（`handle_discovery`，读 `Kernel::discovery_manifest`），并入 OpenAPI；CORS 放行 `GET`。
- **`crates/stross-proto/src/message/endpoint.rs`**：`EndpointSummary` 补 `ToSchema`（供 `DiscoveryResp` 进 OpenAPI）。
- **`scripts/discovery-test.sh`**（新增）：回归 `/api/discovery` 清单正确性 + mDNS/子网扫描收敛到同一 `relay_port`。

**验证（关键，真机）**：
- `cargo test -p stross-kernel` 全绿（99 lib）；`cargo clippy --workspace --all-targets -D warnings` 干净；`cargo fmt --check` 通过。
- PC `/api/discovery` 返回 `relayPort=18777`（与 mDNS 一致）；PC `stross devices` 找到本机 18777 + 手机 8777。
- **手机→PC 反向**（真机 WiFi，下行多播被拦）：手机 logcat 铁证——
  `mDNS 零远端设备，触发子网单播扫描回退` → `子网扫描回退发现 3 台设备` → 设备列表出现 `Stross 设备 192.168.11.61:18777`，且 PC 当时**非广播**（无 mDNS），纯靠子网回退 + `/api/discovery`。

**遗留 / 待定**：
- [ ] 手机端发现的 PC 名在 mDNS（`pico`）与 `/api/discovery`（`Stross 设备`）下不一致——取决于声明名来源（hostname vs identity.device_name），后续统一；
- [ ] 仅跑 `stross relay`（不启动协商）的**纯中继节点**不在 18779，子网扫描探不到（交由 mDNS 发现）——按用户拍板保留此取舍。

### 本轮的模块收敛 + 版本标记

**用户决策**：把散落的发现代码收敛为 **kernel 内单一 `discovery` 模块（`crates/stross-kernel/src/discovery/`）**，并标记为 **v0.2.0**（本模块契约封盘），以便转去完善其它模块。

- 结构：`discovery/mod.rs`（模块文档 + `pub const DISCOVERY_VERSION = "0.2.0"` + 公共 API 重导出）、`discovery/mdns.rs`（mDNS 通告/浏览，原 `discovery.rs`）、`discovery/aggregate.rs`（扫描聚合 + `DiscoveryResp`/`DISCOVERY_PORT`，原 `devices.rs`）。
- 删除 `src/devices.rs` 与 `src/discovery.rs`；`lib.rs` 的 `pub mod devices` 移除，扫描聚合的 crate 级重导出改为 `pub use discovery::{...}` 并随 `discovery` feature。
- 外部消费者（CLI `devices`/`adb status`、GUI `scan_devices` 命令）引用由 `stross_kernel::devices::*` 改为 `stross_kernel::discovery::*`。
- `Kernel::discovery_manifest` / `negotiator::handle_discovery` 的 DTO 引用改为 `crate::discovery::DiscoveryResp`。
- 验证：`cargo test -p stross-kernel --lib` 99 全绿（测试挂到 `discovery::aggregate`/`discovery::mdns`）；`cargo clippy --workspace --all-targets -D warnings` 干净；`cargo fmt --check` 通过；`scripts/discovery-test.sh` 通过（含 mDNS 静默→子网回退生效）。
- **说明**：发现机制与 `Kernel` 强耦合（`discovery_manifest` 读 `relay_ports/device_identity/mdns_info`），故收敛为 kernel 内模块而非独立 crate；若后续要拆独立 crate 需先引入数据提供者 trait（DI）抽象。

### 端口真源收敛（`stross-types::ports`）

- 固定端口（中继 18777 / Android GUI 8777 / 控制面 18778 / 协商+发现 18779 / SRT 33462 / QUIC 33464）**统一为 `stross-types::ports` 单一真源**；kernel 各模块改用 `pub use stross_types::ports::X as Y` 别名保持路径兼容（`relay::DEFAULT_PORT`/`GUI_PORT`、`control::DEFAULT_CTRL_PORT`、`negotiator::DEFAULT_NEGOTIATOR_PORT`、`discovery::DISCOVERY_PORT`、`lib::DEFAULT_SRT/QUIC_PORT`）。
- **消除局部重复**：发现端口与协商端口是同一服务同一端口，`DISCOVERY_PORT` 与 `DEFAULT_NEGOTIATOR_PORT` 现共同指向 `stross_types::ports::NEGOTIATOR_DISCOVERY`，不再各写一份 `18779`。
