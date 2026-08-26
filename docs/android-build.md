# Stross Android 构建指南（2026-08 实测固化）

> 目标：在 Linux（Arch/CachyOS）上从源码产出可安装的 Android APK，并用真机
> 验证「网格发现 → 连接 → 观看」链路。
> 被验证硬件：OPPO PLC110（Android 16, arm64），USB 调试 + 同局域网（mDNS）。

## 1. 工具链（paru 管理，官方源 + AUR）

```bash
paru -S --needed android-tools jdk21-openjdk \
  android-platform-36 android-sdk-build-tools android-sdk-build-tools-36 android-ndk
```

- `android-tools`（官方源）：adb/fastboot，装后直接 `adb` 可用
- `jdk21-openjdk`（官方源）：**Gradle 8.14.3 与系统 JDK 25 不兼容**，构建必须指向
  java-21（`JAVA_HOME=/usr/lib/jvm/java-21-openjdk`）
- AUR：`android-sdk` 基座（/opt/android-sdk，含 sdkmanager）+ `android-platform-36`
  + `android-sdk-build-tools`（37）+ `android-sdk-build-tools-36` + `android-ndk`（r29）
- Rust Android 目标：`rustup target add aarch64-linux-android armv7-linux-androideabi i686-linux-android x86_64-linux-android`
- tauri-cli：`cargo install tauri-cli --locked`（需先有 cargo）

## 2. SDK 许可证（root，一次性）

AUR 包不带已接受的许可证；AGP 想自动装缺的组件时会因许可证未接受而失败
（实测报 `build-tools;35.0.0 ... licences have not been accepted`，来自
`:tauri-android` 子工程，钉 app 的 buildToolsVersion 不生效）：

```bash
sudo bash -c 'yes | JAVA_HOME=/usr/lib/jvm/java-21-openjdk JAVA_TOOL_OPTIONS=-Djava.net.preferIPv4Stack=true /opt/android-sdk/tools/bin/sdkmanager --sdk_root=/opt/android-sdk --licenses && JAVA_HOME=/usr/lib/jvm/java-21-openjdk JAVA_TOOL_OPTIONS=-Djava.net.preferIPv4Stack=true /opt/android-sdk/tools/bin/sdkmanager --sdk_root=/opt/android-sdk "build-tools;35.0.0"'
```

## 3. 构建

```bash
cd apps/stross-gui/src-tauri
JAVA_HOME=/usr/lib/jvm/java-21-openjdk ANDROID_HOME=/opt/android-sdk \
  cargo tauri android build -t aarch64 --apk --debug
# 产物：gen/android/app/build/outputs/apk/universal/debug/*.apk
```

无界面可重试（网络抖动）：

```bash
/tmp/build-apk-retry.sh   # 8 轮重试循环（可用性见 §4.2 网络坑）
```

## 4. 本机环境坑（2026-08 实测）

### 4.1 Clash Verge (Mihomo) TUN fake-IP × JVM 优先 IPv6

- 代理 TUN 的 fake-IP 模式下，DNS 会同时给 A 与 AAAA（198.18.x.x / fdfe::x）。
- 新版 JVM 默认优先 IPv6，连 fake IPv6 到 gradle/maven 域名 → TLS 握手被截断
  （`Remote host terminated the handshake`）；curl 默认 IPv4 所以"看起来能通"。
- 修法：`JAVA_TOOL_OPTIONS=-Djava.net.preferIPv4Stack=true`（daemon 也要拿到；
  改 JAVA_TOOL_OPTIONS 后需 `pkill -f '[G]radleDaemon'` 杀掉旧 daemon 才生效）。

### 4.2 代理对 gradle 大量下载仍概率性断连

- 即使 IPv4，节点对 gradle 成百上千构件的并行拉取仍会间歇断连（curl 单发
  完全正常，~10%/请求）。失败构件不入缓存，重试失败数逐轮递减可收敛；
- 更稳：`~/.gradle/init.d/91-stross-mirror.gradle` 把 maven/google/plugins 仓库
  全量重写为阿里云镜像（dl.google.com 也重写到 aliyun google，只碰一个稳定源）；
  用户决策：工具链 paru 管理、依赖源允许镜像。

### 4.3 其它

- gen/android 的 `app/build.gradle.kts` 钉了 `buildToolsVersion = "36.0.0"`
  （AGP 8.11 默认 35.0.0 会触发自动下载；36 由 paru 提供）。
- 首次 gradle 发行版下载失败（services.gradle.org 被代理黑洞）：用腾讯镜像
  预下载 gradle-8.14.3-bin.zip 放入 `~/.gradle/wrapper/dists/...`（已缓存）。
- 装机：`adb install -r gen/android/app/build/outputs/apk/universal/debug/app-universal-debug.apk`

## 5. 真机验证锚点

- 手机 GUI 打开即自动锚定 + mDNS 扫描（main.ts/grid.ts）；点设备卡片 → 点流卡片 → 观看。
- 屏显统计（watch.ts 轮询 receive_status）：「收到 N 帧 · 解码 N 帧 · 音频 N 块 · 已绘制 N 帧」。
- Rust tracing 打到 logcat：`adb logcat -d | grep stross`。
- 反向验证：手机锚点 /api/info、/api/peers 从 PC 可 curl（手机中继绑定 0.0.0.0）。
- CDP 驱动（无头自动化）：debug 构建的 WebView 暴露 `@webview_devtools_remote_<pid>`
  （`adb forward tcp:9222 localabstract:webview_devtools_remote_<pid>`），
  零依赖 CDP 客户端脚本可 evaluate JS 直接点卡片/读统计（见本轮 /tmp/cdp-eval.py）。

## 6. 真机实测暴露并修复的问题（2026-08-26）

| # | 问题 | 根因 | 修复 |
|---|---|---|---|
| 1 | Android 打开 UI 全空白，JS 全部 `Unexpected token '<'` | `frontendDist` 写成**文件数组**，Android 资源打包未嵌入任何前端文件（APK assets 仅 tauri.conf.json） | 改回目录形式 `"frontendDist": "../web"`（桌面侧同样受益） |
| 2 | 网格出现「点不开」的设备卡片（fe80 条目） | 广播侧 `enable_addr_auto()` 会把网卡全地址（含 fe80 link-local）带进 mDNS；browse 侧取地址集合首项偶取 fe80 | `Discovery::browse` 选址：**过滤 fe80/169.254，优先 IPv4**（双栈 WiFi 下不同设备 IPv6 前缀常不可达，IPv4 是可靠路径）；`broadcast_addrs` 同过滤；前端 `grid.ts` 追加 link-local 剔除 |
| 3 | Android 观看页「解码 N」恒 0、状态卡「等待流数据」 | Android 解码在 Kotlin（MediaCodec），Rust 侧 ReceiveStats 无回写；状态只在无帧时置「等待流数据」、不随帧到达翻回 | `mobile.rs` 解码回调 → `StrossApp::note_android_decoded_frame` → `Receiver::note_decoded_video`；`watch.ts` 轮询在已绘制帧后翻回「接收中」 |
| 4 | 同一设备 IPv4/fe80 多卡片 | 广播携带全部地址、peer 表未去重 | 由 #2 的 browse 选址收敛（每服务选一个可达地址） |