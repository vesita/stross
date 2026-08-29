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
cargo run -p stross-gui

# 同一二进制的无界面中继模式（服务器/常驻部署，不依赖图形环境）
cargo run -p stross-gui -- --relay-only
cargo run -p stross-gui -- --relay-only --port 9000 --no-advertise

# 打包安装包（桌面应用）
cargo tauri build          # 在 apps/stross-gui/src-tauri 下执行

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
| 屏幕 | **Wayland**：XDG Desktop Portal + pipewire（SHM/CPU 路径，合成器无关）；X11：`x11grab` | `gdigrab` |
| 摄像头 | V4L2（`/dev/video*` 自动枚举） | DirectShow（自动枚举） |
| 麦克风 | PulseAudio/PipeWire（`pactl` 枚举） | DirectShow |
| 系统声音 | PulseAudio monitor 源 | 需启用「立体声混音/Stereo Mix」回环设备 |

## Android

构建的**完整实测指南**（工具链安装 / SDK 许可证 / 代理与网络坑 / 真机验证锚点）见
[android-build.md](android-build.md)；此处只给快速路径与能力说明。

### 快速构建

```bash
export JAVA_HOME=<JDK 21 路径>   # 构建必须：系统默认 JDK 25 会让 Kotlin 1.9.25
                                 # 的 buildSrc 崩溃（IllegalArgumentException: 25.0.4.1）
rustup target add aarch64-linux-android
./scripts/setup-android.sh                       # 生成 Android 工程并装配 Kotlin 插件
cd apps/stross-gui/src-tauri && cargo tauri android build --debug -t aarch64
# 产物: gen/android/app/build/outputs/apk/*/debug/app-*-debug.apk
```

> 已在 Linux 上验证：aarch64 debug APK 构建成功（minSdk 24 / targetSdk 36，
> 含 `RECORD_AUDIO`、`FOREGROUND_SERVICE_MEDIA_PROJECTION` 等权限）。

### Android 端能力与注意

- **屏幕推流**：MediaProjection（首次需用户授权）→ MediaCodec H.264，
  API 34+ 通过前台服务获取投影（已在 `ProjectionService.kt` 处理）。
- **麦克风**：AudioRecord → AAC（需要 `RECORD_AUDIO` 运行时权限）。
- **中继地址**：App 内显示本机中继地址（默认 `http://<手机IP>:8777/`）供对端
  手动添加；对端经 mDNS 自动发现时无需手输。
- 摄像头推流（Android）暂未接入，属后续路线图（nokhwa/Camera2）。
- Android 前端入口与桌面共用 `web/`，命令面一致（`start_stream` / `capture_status`）；
  采集由 Rust 侧 `mobile.rs` 的 `AndroidCapture` 实现
  `stross-endpoint::capture::CaptureBackend`（Kotlin 插件经 Channel 回传帧）。

## 问题排查

| 现象 | 排查 |
|---|---|
| 推流端提示「未找到 ffmpeg」 | 安装 ffmpeg 或设置 `STROSS_FFMPEG=/path/to/ffmpeg` |
| Linux 屏幕黑屏/采集不到 | Wayland：确认桌面 portal（xdg-desktop-portal）可用，授权弹窗需允许；X11：确认有 DISPLAY |
| 没有系统声音选项 | Linux：确认 `pactl list short sources` 有 `.monitor` 源；Windows：启用「立体声混音」 |
| 观看端花屏/绿屏 | 关键帧对齐问题，重新订阅/重连（下次关键帧 2s 内自动恢复） |
| 局域网打不开 | 检查防火墙放行端口（GUI 8777；CLI serve 18777 / 协商 18779 / SRT 33462 / QUIC 33464）；确认在同一网段 |
| 延迟较大 | 降低画质档位或减小 `Quality::gop`（默认 2 秒关键帧间隔） |
| Android 构建报 NDK/SDK 错 | 安装 NDK（`sdkmanager "ndk;25.2.9519653"`），设置 `ANDROID_HOME` |
| 桌面应用在 NVIDIA + Wayland 下启动报 `Gdk-Message: Error 71 (协议错误) dispatching to Wayland display` | webkit2gtk 的 DMA-BUF 渲染器与合成器协商失败；应用已自动关闭该渲染器（`WEBKIT_DISABLE_DMABUF_RENDERER=1`，仅 NVIDIA+Wayland 生效）。仍异常可手动 `export WEBKIT_DISABLE_DMABUF_RENDERER=1`；**不要**用 `GDK_BACKEND=x11` 绕过（NVIDIA 下会报 GBM buffer 创建失败） |
