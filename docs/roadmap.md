# Stross 路线图（下一阶段）

> 汇总自实测反馈与架构讨论，按优先级排序。当前版本（0.1.0）已完成五层架构重构
> （proto / core / media / app / sender）与 Android 端到端推流验证。
> 本路线图中「设备路由 / 流解耦 / WebRTC」的统一抽象见
> [plugin-architecture.md](plugin-architecture.md)（内核 + 可插拔传输，三阶段实施）。

## 交互模型愿景：无中枢设备网格（类似投屏，但无"服务器"概念）

```
📱 手机A  ──┐
💻 电脑   ──┼── 设备发现（mDNS）──▶  设备网格：点设备即连，点流即看
📱 手机B  ──┘                         推流自动锚定本机（本地转发点）
                                     → 观看端直连锚点；锚点不可达时
                                       经任意可达中继级联代理（转发链/树）
```

核心变化（2026-09 用户方向）：**淡化"中继/本机"二分**——对用户是设备到设备；
「每流一个转发锚点」由系统自动管理，观看端直连锚点或自动经中继级联，
全程不手输地址、不感知"服务器"。

## TODO（按优先级）

### P0 设备网格拓扑（进行中）

- [x] relay 级联代理（c6263f4）：`POST /api/proxy {upstream, streamId, info?}`
      把上游中继的流拉到本地作虚拟流广播，观看端零改动（复用
      StreamEntry/forward/handle_watch）；`GET /api/proxies` 列出；
      上游断开/失败自动清理；同名冲突 409；修复 handle_watch 悬挂 bug
- [x] 观看端直连失败自动降级（7cc5e41）：`start_receive`/`start_receive_raw`
      直连锚点失败时，自动经本机中继 `start_proxy` 建代理再观看
      （跨网段/防火墙兜底；进程内调用不经 HTTP；无本机中继时直连失败即报错）
- [x] 免先连进入网格：打开应用即自动锚定本机（受控中继 + mDNS 广播）并进入
      「网格」页——本机锚点 / 局域网设备 / 全网串流聚合（含手动添加地址）；
      点设备卡片只看该设备串流；点流卡片 = 按需建立（直连锚点，失败自动
      经本机级联）；推流锚定本机，无需先连接任何设备
- [x] 修复关键帧自愈：`AccessUnitBuilder` 把配置 NAL（SPS/PPS）配给上一帧，
      后续关键帧变"光杆 IDR"——**任何中途接入的观看端（含级联代理）无法解析
      分辨率，解码 0 帧**（本地双设备验证发现：直连早接入 77 帧 vs 中途/级联 0 帧）。
      修复：配置 NAL 归随后的关键帧；回归测试 = `nal.rs` 单测 +
      `tests/nal_ffmpeg_integration.rs`（真实 ffmpeg repeat_headers 流断言
      每个关键帧含 SPS/PPS）
- [x] 修复 CLI 音频链路：`--audio` 此前用 `AudioSourceConfig::default()`
      （synthetic/mic/system_audio 全 None）→ ffmpeg 无音频轨，推流实际无声。
      新增 `AudioSourceConfig::synthetic_test()`（440Hz sine），push/ctrl 两处
      使用；双设备脚本新增音频断言（直连/中途/级联音频块 ≥ 阈值）——
      D3 反向音频的音频链路首次被真实数据验证（sine→AAC→传输→ADTS 解码）
- [x] 本地开发自动化（基础设施）：`scripts/check.sh`（本地 CI：fmt/clippy -D
      warnings/测试/前端类型+同步+jsdom，full|quick|e2e 三档）、
      `scripts/install-hooks.sh`（pre-commit 快速检查）、
      `scripts/build.sh cli|relay|gui|android`（参数化构建）
- [ ] 验证：三设备实测「A 推流 → B 直连看；C 跨网段经 B 中继级联看」
      （单机双实例已覆盖；跨网段需真机，另发现跨设备 SRT/QUIC 拨号格式 bug
      已修：`srt://<ip>:<srtPort>` 不能带 http 端口）

> 约束记录：受控中继只接受内核授权的会话 id（F2.2「先会话后传输」），而
> `/api/*` 无建会话/授权端点 → **向另一台 Stross 设备的受控中继远程推流**
> 目前不可行（推流锚定本机即可被对方发现接收）；远程推流需跨机会话协商
> （见 P2 流解耦 / M3 跨机事件同步协议）。

### P1 手机端原生播放器

> D1 已移除浏览器观看页（无 iframe 依赖）；本阶段为手机端**原生接收**播放器
> （与 iteration-plan 阶段 D1「Android GUI」合并推进）。播放链路 B7 已 Rust 化：
> Android「点流即看」已通（MediaCodec 解码 + AudioTrack 播放 + Rust 侧
> YUV→RGBA/事件规整，见 iteration-plan B7）。

- [x] Android GUI 原生接收：网格页点流即看（MediaCodec 解码 + AudioTrack 播放），
      播放链路 Rust 化（B7）：解码与事件处理不再受 Java 逐像素转换/JSON 数组
      拖累，JNI 直传 Rust 完成 YUV→RGBA 缩放与 base64 事件
- [ ] 播放器可配置：选流、画质、缓冲策略、断线重连
- [ ] 为独立接收 App（stross-viewer）打基础

### P2 流解耦

- [x] 发送/接收角色解耦（随 P0 免先连落地）：推流端与观看端各自独立启动、
      通过发现机制互相找到，不再绑定在「连接 → 推/看」流程里
- [x] 跨设备推流（反向外设：手机麦克风 → 电脑）——**凭证式协商已落地
      （B1）+ GUI 闭环（B2）**：接收端内核建会话并签发一次性 `ShareToken`
      （`ctrl share-token`；GUI 本机卡片「接收手机麦克风」→ `issue_share_token`
      签发展示 PIN/凭证 + 轮询自动接收），推流端 `push --share-token` 或
      GUI「共享麦克风到 TA」（凭证弹窗，QUIC 优先）出示即接入对方受控中继；
      来源感知门控（回环=本机预授权，非回环=必须凭证）杜绝远程冒用预授权；
      凭证推流跳过 `ensure_session` 会话改写（stream_id 必须为接收端签发）；
      Android 纯音频采集走 `micOnly`（跳过屏幕授权，只 AudioRecord→AAC）。
      双 PC 端到端验证脚本 `scripts/share-token-test.sh` 全绿。
      剩余（B3/B4 真机）：电脑扬声器播放手机声音的真机闭环、
      反向音频 ≤200ms 低延迟路径实测
- [ ] 数据面/控制面通道分离：媒体帧（大流量）与控制元数据
      （流列表/状态/心跳）分通道传输

### P3 音视频同步（AV Sync）

- [ ] 现状分析：桌面端视频/音频是两个独立 ffmpeg 进程，各用
      会话起点 `Instant` 打 pts（基准一致）；Android 端两轨
      pts 均来自 MediaCodec `presentationTimeUs`（同一系统时钟）
- [ ] 统一时钟基准：所有轨道用同一单调时钟，跨轨道对齐
- [ ] 接收端校正：接收端基于各轨道首帧做基线对齐，或按 pts
      漂移做动态补偿
- [ ] （远期）若做 RTP/WebRTC 低延迟，需 RTP 时间戳 + RTCP SR 精确同步

### P4 跨网段自动路由（远期）

- [ ] 锚点可达性探测：接收端/中继自动判断直连锚点是否可达，
      不可达时自动选择"最近的"可达中继建立级联（转发树）
- [ ] 发现层扩展：mDNS 覆盖不到的网段，可选 DHT / 手动注册对端
- [ ] 对齐 MoQ：轨道/订阅语义 + 中继链标准化（协议演进方向）

## 已完成的架构基础（2026-08/09）

- [x] 五层模块化：`stross-proto`（协议）/ `stross-core`（局域网共享）/
      `stross-media`（系统适配）/ `stross-app`（核心封装）/ `stross-gui`（UI）
- [x] `CaptureBackend` trait：桌面 ffmpeg 与 Android 原生采集统一抽象，
      命令面两边一致（`start_stream` / `capture_status`）
- [x] Android 端到端验证：屏幕+麦克风推流 → 电脑观看（166 视频帧 + 260 音频帧/5s）
- [x] 修复：前端 `VideoSource` serde 契约（小写 variant）—— 统一命令面后
      桌面与 Android 共用一个 `buildConfig()`
- [x] relay 级联代理（c6263f4）：转发链/树拓扑，详见「P0 设备网格拓扑」
