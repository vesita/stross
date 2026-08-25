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
| A | 当前收口 | 一期验收 1/5 | 已知问题②④ + 三设备实测 + 提交 | 待开始 |
| B | **反向外设（D3 核心验收）** | 一期验收 3、6；§6.1 反向音频 ≤200ms | 跨设备推流：手机麦克风 → 电脑播放 | **下一主攻** |
| C | 弱网与长跑韧性 | §6.2 弱网 5–10% 丢包不冻结 >1s、内存有界 | 弱网测试自动化 + 韧性手段 + 长跑验证 | 待开始 |
| D | 多端互通与网格完善 | 一期验收 2、4、6；F5.1/F5.3 | Android GUI、互通矩阵、会话控制完善 | 待开始 |
| E | 二期无损共享 | F6.1/F6.2 | 文件互传 + 剪贴板同步（ReliableChannel） | 二期 |

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
- [ ] A4 三设备实测（roadmap P0 最后一项）：A 推流 → B 直连看；C 跨网段经 B
      中继级联看（真机；单机双实例已覆盖逻辑，重点验证 mDNS 跨网卡与
      SRT/QUIC 跨设备拨号）
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

### B2 手机端反向外设推流（Android）

- [ ] 手机麦克风采集 →（已有 Android 麦克风推流验证基础）→ 推送到电脑
      受控中继，锚定由"本机"改为"token 指定的接收端"；
- [ ] 手机端 GUI：网格页点电脑设备 →「共享麦克风」→ 输入/扫描凭证 →
      开始推流（F5.1 交互）；
- [ ] 传输自动选择：纯音频 QUIC>WS（已有逻辑，确认跨设备同样生效）；
- 验收：手机推流 30s，电脑收到音频块持续增长，无 0 帧窗口。

### B3 电脑端音频输出 Sink（PlaybackSink）

- [ ] `PlaybackSink` trait 桌面实现（D6 已定）：ffmpeg 子进程解码
      AAC→PCM + cpal 输出扬声器；与采集侧同一二进制、零新增原生依赖；
- [ ] 播放路径接入现有 watch 链路：接收音频帧 → 解码 → 扬声器；
- [ ] （二期增强，WO Mic 路线）录音设备注入：Windows WASAPI 虚拟麦克风 /
      Linux PipeWire 虚拟源——"录音设备播放手机声音"的完整版；
- 验收：电脑扬声器听到手机麦克风声音（D3 验收 3）。

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

### B6 会话控制完善（F2.3/F5.3）

- [ ] 运行中可停止单项共享（停音频不停视频）、断开会话；
- [ ] 运行页实时状态：延迟/帧率/缓冲水位（传输统计推送，D5 已有基础）；
- 验收：一期验收 4（运行中可停止单项共享、断开会话）。

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

## 建议推进顺序

1. **A 阶段**（1 周内）：提交 + ②④ 清理 + 三设备实测——把 P0 收口，获得真机基线；
2. **B 阶段**（主攻）：B1 凭证协议 → B5 抖动缓冲（纯逻辑先行）→ B2/B3 手机推流
   与电脑播放闭环 → B4 延迟压测达标；
3. C/D 与 B 的收尾并行（弱网测试可在 B4 延迟压测后立刻复用同一脚本框架）；
4. E 二期（ReliableChannel）在 B/C 稳定后启动。
