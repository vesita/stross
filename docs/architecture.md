# 架构设计

## 0. 分层架构

五层模块化设计，依赖方向自底向上、单向无环：

```text
┌──────────────────────────────────────────────────────────────────┐
│ apps/stross-gui  ⑤ UI 模块：Tauri 薄命令层 + web/ + android/(Kotlin) │
├──────────────────────────────────────────────────────────────────┤
│ crates/stross-app   ③ 核心封装模块：StrossApp 状态机 / SenderEngine / Kernel │
├───────────────────────────────┬──────────────────────────────────┤
│ crates/stross-core           │ crates/stross-media               │
│ ② 核心局域网共享模块          │ ④ 系统适配模块                    │
│ 中继 / 推流客户端 / mDNS      │ ffmpeg 管线 / 设备枚举            │
│ / 发现引导                    │ / NAL·ADTS 解析 / Source+Sink 能力 │
├───────────────────────────────┴──────────────────────────────────┤
│ crates/stross-transport ①½ 传输插件层：Transport/DataSession 抽象   │
│   ws（无损）/ webrtc（有损）/ srt（自适应）/ quic（无损多路复用）    │
├──────────────────────────────────────────────────────────────────┤
│ crates/stross-proto   ① 协议模块：帧头 + 控制消息（serde）                │
└──────────────────────────────────────────────────────────────────┘
```

| 层 | crate | 职责 | 依赖 |
|---|---|---|---|
| ① 协议 | `stross-proto` | 线上契约：24 字节 v2 帧头（含 seq/分片）+ JSON 控制消息（含能力协商与路由） | 无内部依赖 |
| ①½ 传输 | `stross-transport` | 可插拔传输层：`Transport`/`DataSession` 抽象 + ws/webrtc/srt/quic/memory 实现 + 本机 IP | proto |
| ② 共享 | `stross-core` | 纯数据共享逻辑：中继、推流客户端、mDNS 发现引导（re-export transport/net） | proto + transport |
| ④ 适配 | `stross-media` | 系统适配：ffmpeg 采集管线、设备枚举、H.264/AAC 切帧、`CaptureBackend`（Source）/ `Sink`（录制） | proto |
| ③ 封装 | `stross-app` | 应用状态机 + 引擎组合 + 内核（设备图/会话/路由/鉴权），无 UI 依赖、可单测 | core + media |
| ⑤ UI | `stross-gui` | Tauri 薄命令层 + Web 前端 + Android Kotlin 桥 | app |

> 协议为何保持独立小 crate：共享模块与系统适配模块**都**要使用 `Frame`/`ControlMessage`，
> 独立成 crate 才能让 media 只依赖协议、而不反向依赖共享模块（否则 media 会拉进 axum 等中继依赖）。

### 交互模型

桌面/Android 应用采用「**设备 × 共享流 组合管理**」（P0 免先连 + 界面改版落地）：
打开即自动锚定本机（受控中继 + mDNS 广播）；**设备是实体，共享流是设备之间
的连接实例**。界面为双栏：左「设备」（本机 + 局域网设备卡片，点设备展开 →
发起共享与该设备在线共享），右「共享流」（本机全部活动共享统一管理：

```text
设备（实体）                         共享流（连接实例，统一管理）
┌─────────────────────┐   ┌──────────────────────────────┐
│ 本机（我）            │   │ ↑ 屏幕 → 局域网广播    [停止] │
│  · 共享屏幕/麦克风(广播)│   │ ↑ 麦克风 → 电脑B(凭证)[停止] │
│  · 接收手机麦克风(凭证)│   │ ↓ 麦克风 ← 手机A      [停止] │
│ 电脑B / 手机A …       │   └──────────────────────────────┘
│  · 共享麦克风到 TA     │
│  · TA 的在线共享(点即看)│
└─────────────────────┘
```

- 「锚定本机」：`start_relay` 启动常驻受控中继（接收与推流共用）+ mDNS 广播，
  内核签发会话 id（D4），中继只接受内核授权会话（F2.2）。
- 「共享（出站）」：广播（本机 → 局域网任意接收方，锚定本机中继自动选传输：
  视频 SRT>QUIC>WS、纯音频 QUIC>WS）与定向（凭证式 B2：出示接收端签发的
  `ShareToken` 直推对方受控中继，`ensure_session` 对凭证推流跳过会话改写）。
- 「接收（入站）」：点设备在线共享条目即收（直连锚点，失败自动经本机中继
  级联代理），原生播放（`PlaybackSink`，D6），**无浏览器观看端**（D1）。
- 反向（D3 核心验收）：电脑「接收手机麦克风」→ `issue_share_token` 建会话
  签凭证 → 手机出示凭证推流 → 电脑自动原生接收播放（B2/B3）。

## 1. 总体数据流

```text
推流端（桌面）                                         中继（Rust）                     接收端（桌面原生）
┌─────────────────────────────┐              ┌──────────────────────┐         ┌─────────────────────────┐
│ ffmpeg 视频进程              │              │ axum + tokio          │         │ WS watch 客户端          │
│ 屏幕/摄像头 → H.264 (Annex-B)│──stdout────▶│ /ws/push 收流          │         │ 抖动缓冲(seq/pts 排序)   │
│ ffmpeg 音频进程              │              │ 关键帧缓存             │──广播──▶│ ffmpeg 解码 → cpal 播放  │
│ 麦克风/系统声 → AAC (ADTS)   │──stdout────▶│ /ws/watch?stream=ID    │         │ (PlaybackSink, D6)       │
└─────────────┬───────────────┘              │ /api/streams 流列表    │         └─────────────────────────┘
              │ Rust: NAL/ADTS 切帧          │ 级联代理 /api/proxy    │
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

接收端不再依赖浏览器（D1）：桌面 `PlaybackSink` = ffmpeg 子进程解码 + cpal 输出
（D6）；Android = MediaCodec + AudioTrack。观看端页面（jmuxer/MSE）已移除。

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

`Hello`（推流端声明）→ `Welcome`（中继确认）；接收端连上即收 `Ready`；
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
| `-x264-params repeat_headers=1:slices=1` | 关键帧前重复 SPS/PPS（接收端随时接入）；单 slice（一帧一个 slice，便于切帧） |
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
- **关键帧对齐**：接收端任务只转发关键帧之后的视频帧；掉帧（Lagged）时
  重新等关键帧，避免从 GOP 中间开始导致花屏。
- **最近关键帧缓存**：新观众连上先收到缓存的关键帧（含 SPS/PPS），
  立刻可解码，无需等下一个 GOP。
- 推流端断开（Bye/断连）→ 删流，接收端收到流结束。

## 5. 接收端（原生播放，D1/D6）

浏览器观看端已移除（D1：接收端全部原生，无内嵌 viewer / jmuxer / MSE）。

桌面接收链路（`Receiver` + `PlaybackSink`，见 crates/stross-app/src/receiver.rs、
crates/stross-media/src/playback.rs）：

1. 订阅 `ws://host/ws/watch?stream=ID`（或 SRT/QUIC watch）收媒体帧；
2. 抖动缓冲（SessionDataManager 流式通道）：定长环形缓冲按 seq/pts 索引排序，
   乱序帧落槽等待、按 pts 顺序消费、超时未齐跳过并等关键帧重对齐
   （内存有界 = 固定容量，需求 §4.4）；
3. 视频 → ffmpeg 子进程解码（H.264 → RGBA 原始帧，`RenderedFrame`）交给上层绘制；
   音频 → ffmpeg 子进程解码（AAC → PCM）+ cpal 输出扬声器（D6：与采集侧同一
   ffmpeg 二进制与子进程编排模式，零新增原生构建依赖）。

Android 接收（B7 Rust 化）：编码帧 → Kotlin `PlaybackPlugin`（**MediaCodec/
AudioTrack 系统 API 薄壳**，`feedVideo` 入队立即返回 + 独立解码线程 + 短超时）；
解码输出 YUV 经 **JNI 直传 Rust**（stross-gui `mobile_jni.rs`）——SPS/csd 解析
（`stross_media::nal`）、YUV→RGBA 缩放（`stross_media::yuv`）、base64 事件
`receive-frame`、解码统计回写全部在 Rust 完成，Java 不再做位级解析与逐像素
转换（四重瓶颈根治：同步解码 / 纯 Java 像素循环 / 5s 阻塞 / JSON 数字数组事件）。

> 中继侧"新观众先收最近关键帧 + Lagged 重对齐"机制（§4）与接收端抖动缓冲互补，
> 保证随时接入可解码。

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

## 7. Android 采集（apps/stross-gui/src-tauri/android/）

- `MediaPlugin.kt`：`@TauriPlugin`，`@Command startCapture/stopCapture`。
- 屏幕：`MediaProjectionManager.createScreenCaptureIntent()` 授权 →
  `ProjectionService`（API 34+ 强制的前台服务）→ `getMediaProjection` →
  VirtualDisplay 直连 MediaCodec 输入面（零拷贝）→ H.264 输出。
- 麦克风：`AudioRecord` → AAC MediaCodec → 手动加 ADTS 头。
- 编码帧经 Tauri `Channel`（base64 JSON）回传 Rust `mobile.rs`（`AndroidCapture`
  实现 `CaptureBackend`），转成协议帧送入推流客户端——与桌面端共用同一套中继/接收端。

## 8. 关键设计决策

| 决策 | 理由 |
|---|---|
| ffmpeg 做采集/编码，Rust 做编排 | 跨平台采集最省事且可靠；Rust 专注协议/并发/UI |
| 原始 ES 传输 + 原生端解码 | 统一桌面/Android 两条推流路径；接收端原生解码（D1 移除浏览器/jmuxer） |
| WebSocket 优先，SRT/QUIC 按场景 | 实现简单、穿透局域网无压力；低延迟场景走 UDP 传输（视频 SRT、纯音频 QUIC） |
| 每帧一个 WS 消息 | 接收端按帧统计、按帧对齐，无需解析容器 |
| 中继独立于推流端 | 多机推流到同一中继；受控中继接受内核授权会话（F2.2） |
| 协议独立 crate | media 与 core 都要用 `Frame`，独立成 crate 让适配层不反向依赖共享层 |
| 传输层独立 crate（阶段 2） | 传输实现（str0m/未来 quic/srt）的重依赖不进入 core/media/app 的依赖树；core re-export 保持路径兼容 |
| `CaptureBackend` trait | 桌面 ffmpeg 与 Android 原生采集统一抽象，UI 命令面两边一致 |
| `Sink` trait（阶段 2） | 录制/渲染/注入统一为接收侧能力，与 Source 共用能力描述与协商 |
