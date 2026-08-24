# Stross 路线图（下一阶段）

> 汇总自实测反馈与架构讨论，按优先级排序。当前版本（0.1.0）已完成五层架构重构
> （proto / core / media / app / sender）与 Android 端到端推流验证。
> 本路线图中「设备路由 / 流解耦 / WebRTC」的统一抽象见
> [plugin-architecture.md](plugin-architecture.md)（内核 + 可插拔传输，三阶段实施）。

## 交互模型愿景：设备路由（类似投屏）

```
📱 手机A  ──┐
💻 电脑   ──┼── 设备发现（mDNS）──▶  路由控制：从 [💻 电脑] 推送到 [📱 手机A]
📱 手机B  ──┘                         → 电脑开始采集（屏幕+声音）
                                     → 手机A 原生播放器播放
```

核心变化：设备连接后，可以**控制从什么设备推送到什么设备**，
而不是在任意模式下都要手动指定地址。

## TODO（按优先级）

### P0 设备路由交互（下一步实施）

- [ ] `stross-core`: `RelayServer` 支持附加路由（`start_with(port, extra)`），
      控制 API 与观看页同端口
- [ ] `stross-app`: `StrossApp::start_relay_with(extra)` + 节点信息接口
      （名称/角色/能力/当前流状态）
- [ ] `stross-sender`: 控制 API 路由（`/api/node`、`stream start/stop/status`）
      —— 跨设备控制：设备 A 的 UI 直接调用设备 B 的控制 API，让 B 开始/停止推流
- [ ] mDNS 广播增强：TXT 携带设备角色（sender/viewer）、能力、控制端口；
      发现列表展示设备名而非裸地址
- [ ] UI：设备列表 + 选「源设备 / 目标设备」路由面板
      （本机扫描 → 点选 → 开始推送，全程不手输地址）
- [ ] 验证：两台设备实测「A 控制 B 推送 → 本机观看」

### P1 手机端原生播放器

- [ ] 观看页去掉 iframe 依赖，改为 App 内集成播放器
- [ ] 播放器可配置：选流、画质、缓冲策略、断线重连
- [ ] 为独立观看 App（stross-viewer）打基础

### P2 流解耦

- [ ] 发送/接收角色解耦：推流端与观看端不再绑定在同一个
      「连接 → 推/看」流程里，各自独立启动、通过发现机制互相找到
- [ ] 数据面/控制面通道分离：媒体帧（大流量）与控制元数据
      （流列表/状态/心跳）分通道传输

### P3 音视频同步（AV Sync）

- [ ] 现状分析：桌面端视频/音频是两个独立 ffmpeg 进程，各用
      会话起点 `Instant` 打 pts（基准一致）；Android 端两轨
      pts 均来自 MediaCodec `presentationTimeUs`（同一系统时钟）
- [ ] 统一时钟基准：所有轨道用同一单调时钟，跨轨道对齐
- [ ] 接收端校正：观看端基于各轨道首帧做基线对齐，或按 pts
      漂移做动态补偿
- [ ] （远期）若做 RTP/WebRTC 低延迟，需 RTP 时间戳 + RTCP SR 精确同步

## 已完成的架构基础（2026-08）

- [x] 五层模块化：`stross-proto`（协议）/ `stross-core`（局域网共享）/
      `stross-media`（系统适配）/ `stross-app`（核心封装）/ `stross-sender`（UI）
- [x] `CaptureBackend` trait：桌面 ffmpeg 与 Android 原生采集统一抽象，
      命令面两边一致（`start_stream` / `capture_status`）
- [x] Android 端到端验证：屏幕+麦克风推流 → 电脑观看（166 视频帧 + 260 音频帧/5s）
- [x] 修复：前端 `VideoSource` serde 契约（小写 variant）—— 统一命令面后
      桌面与 Android 共用一个 `buildConfig()`
