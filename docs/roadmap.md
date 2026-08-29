# Stross 路线图（下一阶段）

> 汇总自实测反馈与架构讨论，按优先级排序。已完成的分层架构为
> proto → transport → types → endpoint → kernel → bridge → 壳层（单一
> `Kernel` 门面，见 [layering-architecture.md](layering-architecture.md)）。
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

### P0 设备网格拓扑（已完成，剩三设备实测）

- [x] 免先连进入网格：打开即自动锚定本机（受控中继 + mDNS 广播），全网设备/串流
      聚合（含手动添加地址），点流即看（直连锚点，失败自动经本机中继级联代理兜底）
- [x] relay 级联代理：`POST /api/proxy` 拉上游流作虚拟流广播，观看端零改动
- [x] 关键帧自愈（配置 NAL 归随后的关键帧，中途接入可解码）+ CLI 音频链路修复
      （合成测试音 440Hz）——实现与回归细节见 iteration-plan.md 对应轮次
- [x] 本地开发自动化：`scripts/check.sh`（full|quick|e2e）/ `install-hooks.sh` /
      `scripts/build.sh cli|relay|gui|android`
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

- [x] Android GUI 原生接收：点流即看（MediaCodec 解码 + AudioTrack 播放），
      播放链路 Rust 化（B7）：JNI 直传 Rust 完成 YUV→RGBA 缩放与 base64 事件
- [ ] 播放器可配置：选流、画质、缓冲策略、断线重连
- [ ] 为独立接收 App（stross-viewer）打基础

### P2 流解耦

- [x] 发送/接收角色解耦（随 P0 免先连落地）：推流端与观看端各自独立启动、
      通过发现机制互相找到，不再绑定在「连接 → 推/看」流程里
- [x] 跨设备推流（反向外设：手机麦克风 → 电脑）：凭证式协商（B1）+ GUI 闭环
      （B2）+ 自动协商免粘贴（B2.5），真机闭环（OPPO PLC110 ↔ 本机）：
      接收端建会话签发一次性 `ShareToken`（`ctrl share-token`；GUI 通告「麦克风」
      端点 → 对端订阅）；推流端 `push --share-token` 或订阅端点经 18779 自动申请
      凭证（首次人工确认 + 信任记忆，免确认自动签发）接入对方受控中继；来源感知
      门控（非回环必须凭证）防远程冒用；Android 纯音频走 `micOnly`。
      回归 = `scripts/share-token-test.sh`；细节见 iteration-plan.md 阶段 B。
- [x] 防火墙自动放行（权限自动化，B2.5）：SRT/QUIC 固定端口 33462/33464 +
      `firewall_status` 自检 + `firewall_allow` polkit 一键放行（精确端口 ×
      局域网子网，不再手敲 sudo、不放行整个网段）
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

- [x] 分层模块化：`stross-proto`（协议）/ `stross-transport`（传输）/ `stross-types`（应用契约）/
      `stross-endpoint`（端点）/ `stross-kernel`（内核）/ `stross-bridge`（平台适应）/ `stross-gui`（UI）
- [x] `CaptureBackend` trait：桌面 ffmpeg 与 Android 原生采集统一抽象，
      命令面两边一致（`start_stream` / `capture_status`）
- [x] Android 端到端验证：屏幕+麦克风推流 → 电脑观看（166 视频帧 + 260 音频帧/5s）
- [x] 修复：前端 `VideoSource` serde 契约（小写 variant）—— 统一命令面后
      桌面与 Android 共用一个 `buildConfig()`
