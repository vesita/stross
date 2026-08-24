# 架构设计

## 0. 分层架构

五层模块化设计，依赖方向自底向上、单向无环：

```text
┌──────────────────────────────────────────────────────────────────┐
│ apps/stross-sender  ⑤ UI 模块：Tauri 薄命令层 + web/ + android/(Kotlin) │
├──────────────────────────────────────────────────────────────────┤
│ crates/stross-app   ③ 核心封装模块：StrossApp 状态机 / SenderEngine / Kernel │
├───────────────────────────────┬──────────────────────────────────┤
│ crates/stross-core           │ crates/stross-media               │
│ ② 核心局域网共享模块          │ ④ 系统适配模块                    │
│ 中继 / 推流客户端 / mDNS      │ ffmpeg 管线 / 设备枚举            │
│ / 观看端页面                  │ / NAL·ADTS 解析 / Source+Sink 能力 │
├───────────────────────────────┴──────────────────────────────────┤
│ crates/stross-transport ①½ 传输插件层：Transport/DataSession 抽象   │
│            ws（无损）/ webrtc（有损）/ memory（测试）实现          │
├──────────────────────────────────────────────────────────────────┤
│ crates/stross-proto   ① 协议模块：帧头 + 控制消息（serde）                │
└──────────────────────────────────────────────────────────────────┘
```

| 层 | crate | 职责 | 依赖 |
|---|---|---|---|
| ① 协议 | `stross-proto` | 线上契约：24 字节 v2 帧头（含 seq/分片）+ JSON 控制消息（含能力协商与路由） | 无内部依赖 |
| ①½ 传输 | `stross-transport` | 可插拔传输层：`Transport`/`DataSession` 抽象 + ws/webrtc/memory 实现 + 本机 IP | proto |
| ② 共享 | `stross-core` | 纯数据共享逻辑：中继、推流客户端、mDNS、观看端页面（re-export transport/net） | proto + transport |
| ④ 适配 | `stross-media` | 系统适配：ffmpeg 采集管线、设备枚举、H.264/AAC 切帧、`CaptureBackend`（Source）/ `Sink`（录制） | proto |
| ③ 封装 | `stross-app` | 应用状态机 + 引擎组合 + 内核（设备图/会话/路由/鉴权），无 UI 依赖、可单测 | core + media |
| ⑤ UI | `stross-sender` | Tauri 薄命令层 + Web 前端 + Android Kotlin 桥 | app |

> 协议为何保持独立小 crate：共享模块与系统适配模块**都**要使用 `Frame`/`ControlMessage`，
> 独立成 crate 才能让 media 只依赖协议、而不反向依赖共享模块（否则 media 会拉进 axum 等中继依赖）。

### 交互模型

桌面/Android 应用采用「**先连接，再收/发**」：

```text
连接阶段                    主界面
┌──────────────┐   ┌────────────────────────────┐
│ 本机中继       │──▶│ 📤 推流（发）：采集 → 推流到所连接的中继 │
│ 或 局域网中继  │   │ 📥 观看（收）：内嵌播放器列出中继上的流 │
│ (mDNS 扫描)   │   └────────────────────────────┘
└──────────────┘
```

- 「本机」连接：`start_relay` 启动一个常驻中继（观看与推流共用）。
- 「局域网中继」连接：探测 `/api/streams` 可达性；mDNS 扫描返回候选。
- 推流时把 `relay_url` 指向所连接的中继（桌面内嵌 / 外部中继统一走
  `SenderEngine::start(relay_url)`）。
- 观看页通过 iframe 加载中继托管的观看端页面，复用同一套 MSE 播放器。

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
+--------+---------+-------+-------+---------+---------+---------+----------+----------+----------+----------+
| magic  | version | track | codec | flags   | pts_ms  | seq     | frag_idx | frag_cnt | len      | reserved |
| "STR2" |  u8     |  u8   |  u8   |  u8     | u32 LE  | u32 LE  | u8       | u8       | u32 LE   | u8[2]    |
+--------+---------+-------+-------+---------+---------+---------+----------+----------+----------+----------+
```

- `track`：0 视频 / 1 音频
- `codec`：1 H.264(Annex-B) / 2 AAC(ADTS)
- `flags`：`0x01` 关键帧 / `0x02` 配置数据 / `0x04` 开始 / `0x08` 结束
- `pts_ms`：相对会话起点的演示时间戳
- `seq`：会话内帧序号（有损传输乱序检测；无损传输取 0）
- `frag_idx` / `frag_cnt`：分片位置/总数（`0` = 未分片）

### 控制消息（JSON 文本帧）

`Hello`（推流端声明）→ `Welcome`（中继确认）；观看端连上即收 `Ready`；
`Bye` 结束；`Error` 携带错误。

## 3. 系统适配模块（crates/stross-media）

### 采集后端抽象（capture.rs）

`CaptureBackend` trait 把「把本机媒体源变成 `Frame` 流」抽象成统一接口，
上层引擎/状态机只依赖 trait，不关心平台：

```rust
pub trait CaptureBackend: Send + Sync {
    fn start(&self, cfg: &StreamConfig, tx: mpsc::Sender<Frame>) -> Result<()>;
    fn stop(&self);
    fn status(&self) -> CaptureStatus;
}
```

- 桌面：`FfmpegBackend`（本 crate）—— ffmpeg 子进程采集；
- Android：`AndroidCapture`（UI 层 `mobile.rs` 实现）—— MediaProjection + MediaCodec，
  经 Tauri `Channel` 回传帧；`status()` 由 Kotlin 控制帧（`t=9`）异步回报。

### 桌面采集管线（pipeline.rs）

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

### 设备枚举（devices.rs）

摄像头 / 麦克风 / 系统声音：
- Windows：解析 `ffmpeg -f dshow -list_devices`；
- Linux：`/dev/video*` + sysfs 名称、`pactl` 源列表（monitor = 系统声音）；
- macOS：`avfoundation` 设备列表（尽力而为）。

## 4. 中继（crates/stross-core/src/relay.rs）

- 数据面转发（`handle_push` / `handle_watch`）只依赖传输抽象
  （`stross-transport` 的 `Transport`/`DataSession`），ws 与 webrtc 共用同一套
  关键帧对齐逻辑（见 docs/plugin-architecture.md §4）。
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

## 6. 核心封装模块（crates/stross-app）

### 推流引擎（engine.rs）

`SenderEngine` 把三件事组合起来：

1. 内嵌中继（或连接外部中继）；
2. `RelayClient`（tokio-tungstenite 推流客户端：Hello → 帧 → Bye）；
3. `CaptureBackend`（采集后端，`Arc` 共享注入）。

`SenderEngine::stop()` 顺序：停采集 → 关闭帧通道 → 客户端优雅 Bye → 关中继。

### 应用状态机（app.rs）

`StrossApp` 是命令面的唯一实现，**不依赖任何 UI 框架**：

| 方法 | 说明 |
|---|---|
| `start_relay()` | 启动/复用本机常驻中继 + mDNS 广播 |
| `scan_relays()` | mDNS 扫描局域网中继 |
| `start_stream(cfg, relay_url)` | 组合引擎：外部中继 / 本机中继 / 内嵌中继 |
| `stop_stream()` / `stream_status()` | 推流生命周期 |
| `capture_status()` | 采集真实状态（Android 异步回报） |
| `app_info()` / `list_devices()` | 信息与设备 |

UI 层（桌面 / Android）只把 `invoke` 命令转发到这里，因此命令面两边完全一致：
桌面 `start_stream` 与 Android 走同一条路径，平台差异被 `CaptureBackend` 隔离。

## 7. Android 采集（apps/stross-sender/src-tauri/android/）

- `MediaPlugin.kt`：`@TauriPlugin`，`@Command startCapture/stopCapture`。
- 屏幕：`MediaProjectionManager.createScreenCaptureIntent()` 授权 →
  `ProjectionService`（API 34+ 强制的前台服务）→ `getMediaProjection` →
  VirtualDisplay 直连 MediaCodec 输入面（零拷贝）→ H.264 输出。
- 麦克风：`AudioRecord` → AAC MediaCodec → 手动加 ADTS 头。
- 编码帧经 Tauri `Channel`（base64 JSON）回传 Rust `mobile.rs`（`AndroidCapture`
  实现 `CaptureBackend`），转成协议帧送入推流客户端——与桌面端共用同一套中继/观看端。

## 8. 关键设计决策

| 决策 | 理由 |
|---|---|
| ffmpeg 做采集/编码，Rust 做编排 | 跨平台采集最省事且可靠；Rust 专注协议/并发/UI |
| 原始 ES + 浏览器端封包（jmuxer） | 统一桌面/Android 两条推流路径，无需 Rust 端 MP4 muxer |
| WebSocket 而非 WebRTC | 实现简单、穿透局域网无压力；代价是延迟 1–2s（GOP 级） |
| 每帧一个 WS 消息 | 观看端按帧统计、按帧对齐，无需解析容器 |
| 中继独立于推流端 | 多机推流到同一中继；观看端页面由中继托管 |
| 协议独立 crate | media 与 core 都要用 `Frame`，独立成 crate 让适配层不反向依赖共享层 |
| 传输层独立 crate（阶段 2） | 传输实现（str0m/未来 quic/srt）的重依赖不进入 core/media/app 的依赖树；core re-export 保持路径兼容 |
| `CaptureBackend` trait | 桌面 ffmpeg 与 Android 原生采集统一抽象，UI 命令面两边一致 |
| `Sink` trait（阶段 2） | 录制/渲染/注入统一为接收侧能力，与 Source 共用能力描述与协商 |
