# 架构设计

## 1. 总体数据流

```text
推流端（桌面）                                         中继（Rust）                     观看端（浏览器）
┌─────────────────────────────┐              ┌──────────────────────┐         ┌─────────────────────────┐
│ ffmpeg 视频进程              │              │ axum + tokio          │         │ WebSocket 客户端          │
│ 屏幕/摄像头 → H.264 (Annex-B)│──stdout────▶│ /ws/push 收流          │         │ jmuxer(fMP4 封包)         │
│ ffmpeg 音频进程              │              │ 关键帧缓存             │──广播──▶│ MediaSource (MSE)         │
│ 麦克风/系统声 → AAC (ADTS)   │──stdout────▶│ /ws/watch?stream=ID    │         │ <video> 播放               │
└─────────────┬───────────────┘              │ /api/streams 流列表    │         └─────────────────────────┘
              │ Rust: NAL/ADTS 切帧          │ / 内嵌观看端页面        │
              │ 逐帧打时间戳 → Frame         └──────────────────────┘
              └── WebSocket push ──────────▶        ▲
                                                   │
推流端（Android）                                    │
┌──────────────────────────────┐                    │
│ MediaProjection → MediaCodec  │  Channel(base64)  │
│ AudioRecord → AAC → ADTS 头   │ ──────────────────┘
│ Kotlin 插件 → Rust mobile.rs  │
└──────────────────────────────┘
```

## 2. 协议（crates/stross-proto）

### 媒体帧（二进制 WebSocket 消息）

```
+--------+---------+-------+-------+---------+---------+---------+
| magic  | version | track | codec | flags   | pts_ms  | len     | payload ... |
| "STR1" |  u8     |  u8   |  u8   |  u8     | u32 LE  | u32 LE  |
+--------+---------+-------+-------+---------+---------+---------+
```

- `track`：0 视频 / 1 音频
- `codec`：1 H.264(Annex-B) / 2 AAC(ADTS)
- `flags`：`0x01` 关键帧 / `0x02` 配置数据 / `0x04` 开始 / `0x08` 结束
- `pts_ms`：相对会话起点的演示时间戳

### 控制消息（JSON 文本帧）

`Hello`（推流端声明）→ `Welcome`（中继确认）；观看端连上即收 `Ready`；
`Bye` 结束；`Error` 携带错误。

## 3. 桌面采集管线（crates/stross-core/src/pipeline.rs）

两个 ffmpeg 子进程并行，编码参数刻意选择：

| 参数 | 原因 |
|---|---|
| `-tune zerolatency` | 低延迟编码（无 B 帧、无 lookahead） |
| `-x264-params repeat_headers=1:slices=1` | 关键帧前重复 SPS/PPS（观看端随时接入）；单 slice（一帧一个 slice，便于切帧） |
| `-g <fps*2>` | 2 秒一个关键帧，兼顾延迟与接入速度 |
| 音频 `-f adts` | ADTS 每帧自带采样率/声道配置 |

Rust 侧用 `AnnexBSplitter`（状态机切 NAL）→ `AccessUnitBuilder`
（按 `first_mb_in_slice` 分帧，SPS/PPS 挂在 IDR 帧上）→ 每帧打上
`pts_ms` 推入通道。音频用 `AdtsSplitter` 切 ADTS 帧。

> 注：`-tune zerolatency` 会启用 slice 线程把一帧切成多个 slice，
> 因此必须解析 slice 头部的 `first_mb_in_slice`（Exp-Golomb 首个码字）
> 才能正确分帧——这也是对 Android MediaCodec 多 slice 输出的兜底。

## 4. 中继（crates/stross-core/src/relay.rs）

- 每条流一个 `broadcast::Sender<Bytes>`，容量 1024。
- **关键帧对齐**：观看端任务只转发关键帧之后的视频帧；掉帧（Lagged）时
  重新等关键帧，避免从 GOP 中间开始导致花屏。
- **最近关键帧缓存**：新观众连上先收到缓存的关键帧（含 SPS/PPS），
  立刻可解码，无需等下一个 GOP。
- 推流端断开（Bye/断连）→ 删流，观看端收到流结束。

## 5. 观看端（crates/stross-core/assets/viewer/）

纯静态 HTML/JS/CSS，编译期内嵌进中继（`include_str!`），中继零外部文件。

- 拉取 `/api/streams` 渲染流列表（5 秒轮询）。
- 点选流 → `ws://host/ws/watch?stream=ID` → jmuxer 把 H.264/AAC 原始流
  封成 fMP4 喂给 MSE。
- 断线 3 秒自动重连；界面显示码率/fps/缓冲时长。

## 6. Android 采集（apps/stross-sender/src-tauri/android/）

- `MediaPlugin.kt`：`@TauriPlugin`，`@Command startCapture/stopCapture`。
- 屏幕：`MediaProjectionManager.createScreenCaptureIntent()` 授权 →
  `ProjectionService`（API 34+ 强制的前台服务）→ `getMediaProjection` →
  VirtualDisplay 直连 MediaCodec 输入面（零拷贝）→ H.264 输出。
- 麦克风：`AudioRecord` → AAC MediaCodec → 手动加 ADTS 头。
- 编码帧经 Tauri `Channel`（base64 JSON）回传 Rust `mobile.rs`，
  转成协议帧送入推流客户端——与桌面端共用同一套中继/观看端。

## 7. 推流引擎（crates/stross-core/src/sender.rs）

`SenderEngine` 把三件事组合起来：

1. 内嵌中继（或连接外部中继）；
2. `RelayClient`（tokio-tungstenite 推流客户端：Hello → 帧 → Bye）；
3. `StreamSession`（ffmpeg 子进程 + 读管道）。

`SenderEngine::stop()` 顺序：杀 ffmpeg → 关闭帧通道 → 客户端优雅 Bye → 关中继。

## 8. 关键设计决策

| 决策 | 理由 |
|---|---|
| ffmpeg 做采集/编码，Rust 做编排 | 跨平台采集最省事且可靠；Rust 专注协议/并发/UI |
| 原始 ES + 浏览器端封包（jmuxer） | 统一桌面/Android 两条推流路径，无需 Rust 端 MP4 muxer |
| WebSocket 而非 WebRTC | 实现简单、穿透局域网无压力；代价是延迟 1–2s（GOP 级） |
| 每帧一个 WS 消息 | 观看端按帧统计、按帧对齐，无需解析容器 |
| 中继独立于推流端 | 多机推流到同一中继；观看端页面由中继托管 |
