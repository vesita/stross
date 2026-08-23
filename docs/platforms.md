# 平台构建指南

## 桌面（Linux / Windows）

### 依赖

**Linux（Debian/Ubuntu 系）**

```bash
sudo apt install -y libwebkit2gtk-4.1-dev build-essential curl wget file \
  libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev \
  ffmpeg pulseaudio
```

Arch 系：`sudo pacman -S webkit2gtk-4.1 gtk3 ffmpeg pulseaudio`

**Windows**

- 安装 [ffmpeg](https://ffmpeg.gyan.dev/) 并加入 PATH（或设置 `STROSS_FFMPEG`）
- 安装 Rust（`rustup`），Visual Studio Build Tools（C++ 桌面开发工作负载）

### 构建 / 运行

```bash
# 推流端（Tauri 桌面应用）—— PC 端整合的单一应用
cargo run -p stross-sender

# 同一二进制的无界面中继模式（服务器/常驻部署，不依赖图形环境）
cargo run -p stross-sender -- --relay-only
cargo run -p stross-sender -- --relay-only --port 9000 --no-advertise

# 打包安装包（桌面应用）
cargo tauri build          # 在 apps/stross-sender/src-tauri 下执行

# 可选：独立中继组件（多机部署场景）
cargo run -p stross-relay -- -p 8777
cargo run -p stross-relay --features discovery -- -p 8777 --advertise
```

> `--advertise` 需要以 `--features discovery` 构建中继：
> `cargo run -p stross-relay --features discovery -- -p 8777 --advertise`
> （mDNS 广播 `_stross._tcp`，局域网内其它 Stross 实例可发现它。
> 桌面应用与本机中继默认开启 mDNS 广播，无需额外参数。）

### 桌面采集支持矩阵

| 源 | Linux | Windows |
|---|---|---|
| 屏幕 | X11（`x11grab`）；Wayland 需 XWayland | `gdigrab` |
| 摄像头 | V4L2（`/dev/video*` 自动枚举） | DirectShow（自动枚举） |
| 麦克风 | PulseAudio/PipeWire（`pactl` 枚举） | DirectShow |
| 系统声音 | PulseAudio monitor 源 | 需启用「立体声混音/Stereo Mix」回环设备 |

## Android

### 前置条件

- Android SDK（`ANDROID_HOME`）+ NDK（Tauri 要求）
- JDK 17+
- Rust Android 目标：

```bash
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
```

### 装配与构建

```bash
# 1) 生成 Android 工程并装配 Kotlin 插件（权限、前台服务、MainActivity）
./scripts/setup-android.sh

# 2) 构建 APK（可加 --target aarch64 只编 arm64，加快速度）
cd apps/stross-sender/src-tauri
cargo tauri android build --apk --debug
```

APK 输出：`apps/stross-sender/src-tauri/gen/android/app/build/outputs/apk/`

> 已在 Linux 上验证：aarch64 debug APK 构建成功（minSdk 24 / targetSdk 36，
> 含 `RECORD_AUDIO`、`FOREGROUND_SERVICE_MEDIA_PROJECTION` 等权限）。
> 需要 JDK 17+、Android SDK（platform 36、build-tools 36、NDK 27）与 Rust Android 目标。

### Android 端能力与注意

- **屏幕推流**：MediaProjection（首次需用户授权）→ MediaCodec H.264，
  API 34+ 通过前台服务获取投影（已在 `ProjectionService.kt` 处理）。
- **麦克风**：AudioRecord → AAC（需要 `RECORD_AUDIO` 运行时权限）。
- **观看地址**：App 内显示 `http://<手机IP>:8777/`，局域网设备浏览器直接打开。
- 摄像头推流（Android）暂未接入，属后续路线图（nokhwa/Camera2）。
- Android 前端入口与桌面共用 `web/`，命令面一致（`start_stream` / `capture_status`）；
  采集由 Rust 侧 `mobile.rs` 的 `AndroidCapture` 实现
  `stross-media::CaptureBackend`（Kotlin 插件经 Channel 回传帧）。

## 问题排查

| 现象 | 排查 |
|---|---|
| 推流端提示「未找到 ffmpeg」 | 安装 ffmpeg 或设置 `STROSS_FFMPEG=/path/to/ffmpeg` |
| Linux 屏幕黑屏/采集不到 | 使用 X11 会话；Wayland 下确保应用运行在 XWayland |
| 没有系统声音选项 | Linux：确认 `pactl list short sources` 有 `.monitor` 源；Windows：启用「立体声混音」 |
| 观看端花屏/绿屏 | 关键帧对齐问题，刷新页面重新接入（3 秒自动重连） |
| 局域网打不开 | 检查防火墙放行端口（默认 8777）；确认在同一网段 |
| 延迟较大 | 降低画质档位或减小 `Quality::gop`（默认 2 秒关键帧间隔） |
| Android 构建报 NDK/SDK 错 | 安装 NDK（`sdkmanager "ndk;25.2.9519653"`），设置 `ANDROID_HOME` |
