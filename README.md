# 📡 Stross — 局域网设备共享（一站式）

用 **Rust** 实现的局域网（LAN）一站式设备共享：设备自动发现，协商后把本机的
**屏幕、摄像头、麦克风、系统声音**以流式方式共享给目标设备；**任何设备既可是源也可是汇**
（"电脑用手机麦克风"是核心验收场景）。接收端全部原生（桌面 ffmpeg 解码 / Android
MediaCodec），**无需浏览器**。

| | 支持平台 | 说明 |
|---|---|---|
| **推流端** | Linux / Windows / Android | 桌面端用 ffmpeg 采集编码；Android 用 MediaProjection + MediaCodec 原生采集 |
| **接收端** | Linux / Windows / Android | 原生接收播放（ffmpeg 解码 / Kotlin MediaCodec），无需安装额外软件 |

> 借鉴的开源项目：媒体管线与中继模型参考 [OBS](https://github.com/obsproject/obs-studio) 和
> [MediaMTX](https://github.com/bluenviron/mediamtx)；Android 屏幕采集参考
> [scrcpy](https://github.com/Genymobile/scrcpy)；浏览器端 H.264/AAC 封包复用
> [jmuxer](https://github.com/webstream-labs/jmuxer)；mDNS 发现使用 [mdns-sd](https://crates.io/crates/mdns-sd)。

## 快速开始（桌面）

### 前置依赖

- Rust 1.88+（`rustup` 安装，跟随默认 stable 工具链即可）
- ffmpeg（含 libx264；`STROSS_FFMPEG` 环境变量可指定路径）
- Linux 还需要：`webkit2gtk-4.1`、`gtk3`（Tauri 依赖）、PulseAudio/PipeWire（音频采集）

### 运行推流端

```bash
cargo run -p stross-gui          # 桌面应用（Tauri）
```

**PC 端是整合的单一应用**（`stross-gui`），两种模式：

```bash
stross-gui                      # 桌面应用：打开即进入设备网格（免先连）
stross-gui --relay-only         # 无界面中继（服务器/常驻部署，不依赖图形环境）
stross-gui --relay-only --port 9000 --no-advertise   # 自定义端口 / 关闭 mDNS 广播
```

应用采用「**免先连进入网格**」的交互（无"服务器"概念，设备到设备）：

1. **打开即锚定本机**：自动启动受控中继 + mDNS 广播，本机成为网格中的一个锚点
   （推流锚定本机，无需先连接任何设备）；
2. **网格页**：自动扫描局域网设备（mDNS）并聚合各设备的在线串流
   （手动添加地址也可）；点设备卡片只看该设备的串流；
3. **点流即看**：点串流卡片 = 按需建立连接——直连该设备锚点，
   直连失败自动经本机中继**级联代理**兜底（跨网段/防火墙）；
4. **推流（发）**：选屏幕/摄像头/麦克风/系统声音，点「开始推流」即锚定本机，
   局域网设备在网格页发现本机串流即可接收。

> `stross-relay` 保留为**可选的服务器组件**（独立部署、多机中继），
> 日常使用 PC 端一个应用即可。

### 独立中继（可选）

日常使用 PC 端一个应用即可（桌面模式内嵌中继，或 `--relay-only` 无界面模式）。
需要独立部署中继时（如树莓派/NAS 常驻、多机推流到同一中继）：

```bash
cargo run -p stross-relay -- -p 8777
cargo run -p stross-relay -- -p 8777 --advertise   # 需要 discovery feature，见 docs/platforms.md
```

## 架构

分层模块化设计（依赖方向自底向上，单向无环）：

```
┌────────────────────────────────────────────────────────────┐
│ apps/stross-cli / stross-gui  ⑥ UI 壳层：参数解析 + 展示 + 平台适配 │
├────────────────────────────────────────────────────────────┤
│ crates/stross-bridge    ⑤ 平台适应桥接层：paths / hostname / 平台设备枚举 │
├────────────────────────────────────────────────────────────┤
│ crates/stross-kernel   ★ 内核：全部平台无关服务（单一 Kernel 门面）   │
│   数据面 relay{srv,client}/sender/watch/jitter/discovery       │
│   信令 control/negotiator/subscriber/file_xfer/bootstrap       │
│   devices(扫描)/engine(推流)/receiver(接收)/kernel(会话·路由·端点) │
├──────────────────────────────┬─────────────────────────────┤
│ crates/stross-media          │ （transport/proto 在下方）    │
│ ④ 能力层：采集/播放/管线/设备枚举 │                             │
├──────────────────────────────┴─────────────────────────────┤
│ crates/stross-transport   ①½ 传输插件层：Transport/DataSession 抽象      │
│            ws / webrtc / srt / quic 实现                      │
├────────────────────────────────────────────────────────────┤
│ crates/stross-proto        ① 协议模块：帧头 + 控制消息（serde）        │
└────────────────────────────────────────────────────────────┘
```

- **① 协议模块**（`stross-proto`）：线上契约（24 字节 v2 帧头 + JSON 控制消息，
  含能力协商与路由控制），保持独立小 crate —— 能力层与内核都依赖它，但互不依赖。
- **①½ 传输插件层**（`stross-transport`）：可插拔传输抽象（`Transport`/`DataSession`）
  与实现 —— ws（无损，现状）、webrtc（有损低延迟，str0m datachannel）、
  srt（自适应，rsrt 纯 Rust）、quic（无损多路复用，quinn）。
  `stross-kernel` re-export 保持路径兼容。
- **② 内核**（`stross-kernel`）：**全部平台无关服务**，单一门面
  [`Kernel`](crates/stross-kernel/src/kernel/mod.rs) —— 中继服务器 + 中继 HTTP
  客户端（契约单一真源）、mDNS 发现、控制面、凭证协商、订阅/文件传输、引导、
  端点框架（会话/路由/鉴权）、推流引擎与接收编排。不含任何路径/OS/平台代码。
- **④ 能力层**（`stross-media`）：把"本机媒体源变成协议帧"的能力抽象 ——
  ffmpeg 采集管线、设备枚举、H.264/AAC 流切帧，以及统一的
  [`CaptureBackend`](crates/stross-media/src/capture.rs) trait（Source）与
  [`Sink`](crates/stross-media/src/sink.rs) trait（录制/注入）。
- **⑤ 平台适应桥接层**（`stross-bridge`）：数据目录解析 / 主机名 / 平台设备
  静态枚举 —— 只产出**参数**注入内核（base_dir / hostname / 设备清单），
  不持有状态、不定义协议。
- **⑥ UI 壳层**（`apps/stross-gui` / `apps/stross-cli`）：Tauri 壳只做两件事 ——
  把 `Kernel` 注入托管状态、把前端命令转发给它；Android 原生采集以
  `CaptureBackend` 实现（`mobile.rs`）藏在能力层后面，命令面与桌面完全一致。

数据流：

```
┌──────────────┐   H.264/AAC 原始流    ┌─────────┐   H.264/AAC      ┌────────────────┐
│ 推流端        │ ── WebSocket push ──▶ │ 中继     │ ── broadcast ─▶ │ 接收端（原生）    │
│ (CaptureBackend)│  (逐帧 + 时间戳)    │ (Rust)  │   (关键帧对齐)  │ (ffmpeg/MediaCodec)│
└──────────────┘                       └─────────┘                  └────────────────┘
```

- **推流端**：桌面 = `FfmpegBackend`（ffmpeg 子进程：视频 H.264 Annex-B、音频 AAC ADTS）；
  Android = `AndroidCapture`（Kotlin 插件 MediaProjection + MediaCodec 经 Channel 回传帧）。
- **中继**：tokio + axum，`/ws/push` 收流、`/ws/watch` 广播、`/api/streams` 列流、
  `/api/proxy` 级联代理。新观众**先收到最近关键帧**再对齐播放。
- **接收端**：WS/SRT/QUIC watch → 抖动缓冲（SessionDataManager）→ 原生解码播放
  （桌面 ffmpeg PlaybackSink；Android Kotlin MediaCodec）；直连锚点失败时自动经
  本机中继级联代理兜底。

详细设计见 [docs/architecture.md](docs/architecture.md)、[docs/protocol.md](docs/protocol.md)。
下一阶段规划（设备路由 / 原生播放器 / AV 同步）见 [docs/roadmap.md](docs/roadmap.md)。
内核 + 可插拔传输的插件化架构设计见 [docs/plugin-architecture.md](docs/plugin-architecture.md)；
分层判据见 [docs/layering-architecture.md](docs/layering-architecture.md)。

## 目录结构

```
crates/
  stross-proto/      ① 协议：帧头 + 控制消息（serde）
  stross-transport/  ①½ 传输插件层：Transport/DataSession + ws/webrtc/srt/quic 实现
  stross-kernel/     ② 内核：全部平台无关服务（单一 Kernel 门面）
    src/relay/        中继：mod（转发）/ http（路由·API·信令）/ client / peers
    src/kernel/       门面：mod（Kernel）/ graph / session / auth / endpoint / data_plane
    src/              控制面 control / 协商 negotiator / 订阅 subscriber / 引导 bootstrap …
  stross-media/      ④ 能力层：ffmpeg 管线 / 设备枚举 / NAL·ADTS / CaptureBackend / Sink
    src/pipeline/     管线：mod（配置·会话）/ args（ffmpeg 命令构建）
  stross-bridge/     ⑤ 平台适应：paths（数据目录）/ hostname / 平台设备枚举
apps/
  stross-relay/      独立中继二进制（纯 Rust，薄壳）
  stross-gui/     ⑤ UI：Tauri 客户端（桌面 + Android）
    src-tauri/
      android/       Kotlin 插件源码（MediaProjection + MediaCodec）
      src/mobile.rs  Android 采集后端桥（CaptureBackend 实现）
    web/             客户端界面（TS 真源 → app.js 构建产物）
scripts/
  setup-android.sh   Android 工程装配脚本
  check-frontend.sh  前端 app.js 防漂移检查
```

> 命名约定：`apps/` 放二进制（`stross-relay` 中继、`stross-gui` 客户端），
> `crates/` 放库；大模块超过约 500 行时拆为目录（`mod.rs` + 领域子文件）。

## 平台指南

- [Linux / Windows 桌面构建与使用](docs/platforms.md#桌面-linux--windows)
- [Android 构建与运行](docs/platforms.md#android)
- [问题排查](docs/platforms.md#问题排查)

## 测试

```bash
cargo test --workspace          # 单元 + 集成测试
cargo test -p stross-kernel --test sender_e2e -- --nocapture   # 真实 ffmpeg 端到端
npx -y -p "typescript@5.9.3" tsc -p apps/stross-gui/web/tsconfig.json  # 前端类型检查（改 app.ts 后）

scripts/check.sh                # 本地全量检查：fmt + clippy(-D warnings) + 测试 + 前端
scripts/check.sh --quick        # 提交前快速检查（秒级）
scripts/check.sh --e2e          # 追加双设备端到端（直连/中途/级联）
scripts/install-hooks.sh        # 安装 pre-commit 钩子（每次提交自动快速检查；--remove 卸载）
scripts/build.sh cli|relay|gui|android   # 参数化构建（可选 --release）
node scripts/test-frontend.mjs  # 前端网格交互无头测试（jsdom，24 项断言，自动拉依赖）
scripts/dual-device-test.sh     # 本地双设备端到端：直连 / 中途接入 / 级联代理 三段接收全解码
```

## 路线图

- [x] 桌面推流（屏幕/摄像头/麦克风/系统声音）
- [x] Android 屏幕 + 麦克风推流（Kotlin 插件）
- [x] 原生接收播放（D1 去浏览器观看端；桌面 ffmpeg / Android MediaCodec）
- [x] 免先连设备网格：打开即见全网设备/流，点流即看，级联代理兜底
- [ ] 跨设备推流（反向外设：手机麦克风 → 电脑，需跨机会话协商）
- [ ] WebRTC 低延迟（<300ms）通道
- [ ] 摄像头推流（Android，nokhwa/Camera2）
- [ ] 无损共享（文件/剪贴板，二期）
- [ ] 推流鉴权 / 多流房间

## License

MIT OR Apache-2.0
