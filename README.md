# 📡 Stross — 局域网串流器

用 **Rust** 实现的局域网（LAN）实时串流工具：把电脑/手机的**屏幕、摄像头、麦克风、系统声音**
实时推流到局域网，任意设备（手机、平板、电脑）用浏览器打开一个网址即可观看。

| | 支持平台 | 说明 |
|---|---|---|
| **推流端** | Linux / Windows / Android | 桌面端用 ffmpeg 采集编码；Android 用 MediaProjection + MediaCodec 原生采集 |
| **观看端** | 任意带浏览器的设备 | 内嵌 Web 播放器（MSE 解码），无需安装任何软件 |

> 借鉴的开源项目：媒体管线与中继模型参考 [OBS](https://github.com/obsproject/obs-studio) 和
> [MediaMTX](https://github.com/bluenviron/mediamtx)；Android 屏幕采集参考
> [scrcpy](https://github.com/Genymobile/scrcpy)；浏览器端 H.264/AAC 封包复用
> [jmuxer](https://github.com/webstream-labs/jmuxer)；mDNS 发现使用 [mdns-sd](https://crates.io/crates/mdns-sd)。

## 快速开始（桌面）

### 前置依赖

- Rust 1.80+（`rustup` 安装）
- ffmpeg（含 libx264；`STROSS_FFMPEG` 环境变量可指定路径）
- Linux 还需要：`webkit2gtk-4.1`、`gtk3`（Tauri 依赖）、PulseAudio/PipeWire（音频采集）

### 运行推流端

```bash
cargo run -p stross-sender          # 桌面应用（Tauri）
```

应用采用「**先连接，再收/发**」的交互：

1. **连接**：选择「本机」（自动启动一个内嵌中继）或「局域网中继」
   （输入地址，或用 mDNS 扫描局域网内的中继）；
2. **推流（发）**：选屏幕/摄像头/麦克风/系统声音，点「开始推流」，
   画面推送到所连接的中继；
3. **观看（收）**：切到「观看」页，内嵌播放器直接列出该中继上的
   在线串流，点选即看。

同时，局域网内任意设备（手机 / 平板 / 电脑）用浏览器打开中继地址：

```
http://192.168.1.100:8777/
```

无需安装任何软件即可观看。

### 独立中继（可选）

不依赖推流端的 GUI，单独跑一个中继，多台机器可以推到同一个中继：

```bash
cargo run -p stross-relay -- -p 8777
```

然后任意推流端（桌面应用支持「推到外部中继」，代码见 `SenderEngine::start(relay_url)`）推送。

## 架构

```
┌──────────────┐   H.264/AAC 原始流    ┌─────────┐   H.264/AAC      ┌──────────────┐
│ 推流端        │ ── WebSocket push ──▶ │ 中继     │ ── broadcast ─▶ │ 观看端(浏览器) │
│ (Rust 编排)  │   (逐帧 + 时间戳)     │ (Rust)  │   (关键帧对齐)  │ (MSE + jmuxer)│
└──────────────┘                       └─────────┘                  └──────────────┘
```

- **推流端**：桌面 = ffmpeg 子进程（视频 H.264 Annex-B、音频 AAC ADTS）→ Rust 解析成帧；
  Android = Kotlin 插件（MediaProjection + MediaCodec）经 Channel 回传帧。
- **中继**：tokio + axum，`/ws/push` 收流、`/ws/watch` 广播、`/api/streams` 列流、
  `/` 内嵌观看端页面。新观众**先收到最近关键帧**再对齐播放。
- **观看端**：WebSocket 收帧 → jmuxer 封成 fMP4 → MSE 播放，支持断线自动重连。

详细设计见 [docs/architecture.md](docs/architecture.md)、[docs/protocol.md](docs/protocol.md)。

## 目录结构

```
crates/
  stross-proto/      线上协议：帧头 + 控制消息（serde）
  stross-core/       核心库：ffmpeg 管线、NAL/ADTS 解析、WS 中继、mDNS、设备枚举
apps/
  stross-relay/      独立中继二进制（纯 Rust）
  stross-sender/     Tauri 推流端（桌面 + Android）
    src-tauri/
      android/       Kotlin 插件源码（MediaProjection + MediaCodec）
      web/           推流端界面（零构建步骤的 HTML/JS/CSS）
scripts/
  setup-android.sh   Android 工程装配脚本
crates/stross-core/assets/viewer/   观看端页面（编译期内嵌进中继）
```

## 平台指南

- [Linux / Windows 桌面构建与使用](docs/platforms.md#桌面-linux--windows)
- [Android 构建与运行](docs/platforms.md#android)
- [问题排查](docs/platforms.md#问题排查)

## 测试

```bash
cargo test --workspace          # 单元 + 集成测试
cargo test -p stross-core --test sender_e2e -- --nocapture   # 真实 ffmpeg 端到端
```

## 路线图

- [x] 桌面推流（屏幕/摄像头/麦克风/系统声音）
- [x] 浏览器观看端（MSE，延迟 ≈ 1–2 秒）
- [x] Android 屏幕 + 麦克风推流（Kotlin 插件）
- [ ] WebRTC 低延迟（<300ms）通道
- [ ] 摄像头推流（Android，nokhwa/Camera2）
- [ ] 观看端原生 App（复用同一协议）
- [ ] 推流鉴权 / 多流房间

## License

MIT OR Apache-2.0
