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
> [scrcpy](https://github.com/Genymobile/scrcpy)；mDNS 发现使用 [mdns-sd](https://crates.io/crates/mdns-sd)（本地 fork：`crates/mdns`）。

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

1. **打开即锚定本机**：自动启动受控中继 + mDNS 广播，本机成为网格中的一个锚点
   （推流锚定本机，无需先连接任何设备）；
2. **管理与消费解耦**：状态机驱动（FSM）的 UI 架构，清晰分离「设备树/端点共享管理」与「多流消费播放台」；
3. **网格页**：自动扫描局域网设备（mDNS）并聚合各设备的在线串流
   （手动添加地址也可）；点设备卡片只看该设备的串流；
4. **点流即看（多流并发）**：点串流卡片 = 按需建立连接——直连该设备锚点，
   直连失败自动经本机中继**级联代理**兜底（跨网段/防火墙）；支持屏幕 + 系统声多流并发同播；
5. **智能播放交互**：左滑亮度 / 右滑音量 / 双击切换比例 / 双指缩放，全屏根据视频宽高比智能自适应横竖屏；
6. **推流（发）**：选屏幕/摄像头/麦克风/系统声音，默认 1080p@30fps（6Mbps）高清流，点「开始推流」即锚定本机，
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

分层模块化设计（依赖方向自底向上、单向无环）；分层判据与红线见
[docs/layering-architecture.md](docs/layering-architecture.md)：

| crate | 职责 |
|---|---|
| `stross-proto` | 线协议类型（24 字节 v2 帧头 + JSON 控制消息 + 协商握手/L2 目录） |
| `stross-transport` | 可插拔传输抽象（`Transport`/`DataSession`）+ ws / webrtc / srt / quic 实现 |
| `stross-endpoint` | 数据源/宿插件区：Endpoint 契约（load/share）+ screen/audio/file + 采集/播放机制 |
| `stross-types` | 应用契约单一真源（展示视图 / 控制面载荷 / DTO；依赖只到 proto） |
| `stross-kernel` | ★ 全部平台无关服务，单一 [`Kernel`](crates/stross-kernel/src/kernel/mod.rs) 门面 |
| `stross-bridge` | 平台适应：paths / hostname / 平台端点构造（只产出参数，不持状态） |
| `apps/*` | 壳层：参数解析 + 展示 + 平台适配（cli / gui / relay） |

数据流：推流端采集（桌面 ffmpeg / Android MediaProjection+MediaCodec）→ 逐帧打
时间戳经 WS/SRT/QUIC push 进中继（tokio+axum：关键帧对齐 + 最近关键帧缓存）→
观看端 watch 收流 → 抖动缓冲 → 原生解码播放（桌面 ffmpeg+cpal / Android
MediaCodec+AudioTrack）；直连锚点失败自动经本机中继级联代理兜底。
链路细节见 [docs/architecture.md](docs/architecture.md) §1。

详细设计：线上协议见 [docs/protocol.md](docs/protocol.md)；
端点框架规格（节点→端点→策略三层注册 + 分享/订阅双特性）见 [docs/endpoint-model-v2.md](docs/endpoint-model-v2.md)；
可插拔传输设计见 [docs/plugin-architecture.md](docs/plugin-architecture.md)；
下一阶段规划见 [docs/roadmap.md](docs/roadmap.md)。

## 目录结构

```
crates/
  stross-proto / stross-transport / stross-types / stross-endpoint /
  stross-kernel / stross-bridge / mdns（mdns-sd 本地 fork）
apps/
  stross-cli        命令行（serve/ctrl/devices/adb/push/receive/relay/endpoint）
  stross-gui        Tauri GUI（桌面 + Android 共用 web 前端）
  stross-relay      独立中继
scripts/            构建 / 测试 / 真机回归脚本
docs/               设计文档（docs/README.md 是索引）
```

> 命名约定：`apps/` 放二进制，`crates/` 放库；大模块超过约 500 行时拆为目录
> （`mod.rs` + 领域子文件）。各 crate 内部结构见 [AGENTS.md](AGENTS.md) §1。

## 平台指南

- [Linux / Windows 桌面构建与使用](docs/platforms.md#桌面-linux--windows)
- [Android 构建与运行](docs/platforms.md#android)
- [问题排查](docs/platforms.md#问题排查)

## 测试

```bash
scripts/check.sh [--quick|--e2e]   # 本地全量 / 提交前快速 / 双设备端到端检查
cargo test --workspace             # 单元 + 集成测试（含真实 ffmpeg 端到端）
```

回归脚本清单（双设备 / 弱网 / 延迟 / 凭证 / 断连回收 / 前端无头）见
[AGENTS.md](AGENTS.md) §5；`scripts/install-hooks.sh` 可装 pre-commit 钩子
（每次提交自动跑快速检查，`--remove` 卸载）。

## 路线图

- [x] 桌面推流（屏幕/摄像头/麦克风/系统声音，默认 1080p 6Mbps 高清流）
- [x] Android 屏幕 + 麦克风推流（Kotlin 插件 + MediaProjection 原生采集）
- [x] Android 标准播放器最佳实践（Keep-Screen-On 常亮、AudioFocus 智能避让/自动渐变 Ducking、低延迟 MediaCodec/AudioTrack）
- [x] 原生接收播放（D1 去浏览器观看端；桌面 ffmpeg / Android MediaCodec 硬件直解）
- [x] UI 状态机架构（FSM）与响应式解耦（管理视图 vs 消费播放台）
- [x] 智能播放器交互（手势 HUD 调节亮度/音量/画面比例/双指缩放、全屏智能自适应旋转横竖屏）
- [x] AI 智能多流拓扑布局与实时遥测诊断（FPS 滑动滤波、抖动与丢包健康度评估）
- [x] 免先连设备网格：打开即见全网设备/流，点流即看，级联代理兜底
- [x] 跨设备推流（反向外设：手机麦克风 → 电脑，凭证式协商 B1/B2 + 免粘贴 B2.5 真机闭环）
- [x] WebRTC 低延迟通道（transport-webrtc 已落地；局域网端到端延迟 SRT/QUIC 已实测 ≤200ms）
- [x] 推流鉴权（受控中继 + 一次性 ShareToken 凭证 + 来源感知门控）
- [x] 文件端点（确定目标端点，订阅→推送联动；`dual-node-file-test.sh` 本地双端验证）
- [x] 高性能 Rust 像素处理（12 位定点双线性缩放 + YUV/RGBA 转换）
- [ ] 摄像头推流（Android，nokhwa/Camera2）
- [ ] 剪贴板同步（二期无损共享剩余项）
- [ ] 多流房间 / 命名空间（一期后评估）
## License

MIT OR Apache-2.0
