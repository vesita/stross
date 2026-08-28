# AGENTS.md — Stross 开发指南

Stross 是一个用 Rust 编写的局域网流媒体工具箱（无中枢分布式设备网格）：
设备间经局域网直连或级联中继，流式优先 UDP 系传输（SRT / QUIC / WebRTC，
WS 兜底），支持屏幕 / 摄像头 / 麦克风 / 系统声音共享。

本文件为 AI 编码代理与本仓库协作的常驻指南：先读它，再动手。

---

## 1. 仓库布局与分层

```
crates/
  stross-proto      线协议类型（消息/帧/时间；含协商握手与 L2 目录：
                    message/negotiator.rs）
  stross-transport  传输层：SRT / QUIC / WS / WebRTC、RelayUrl、
                    net（local_ips / advertise_ip / fake-IP 判定）
  stross-types      应用契约层：跨壳层类型单一真源（展示视图 AppInfo/RelayInfo/
                    StreamStatus…、控制面载荷 CtrlPayload、DTO CameraDevice/
                    PendingRequest/MediaSubscribeOutcome；依赖只到 proto）
  stross-kernel     ★ 内核（全部平台无关服务，单一门面 Kernel）：
                    中继 server + 中继 HTTP 客户端（relay/，契约单一真源）、
                    mDNS Discovery、sender/watch/jitter、控制面 CtrlServer
                    + client（D7）、凭证协商 ShareNegotiator + client、
                    端点框架 kernel/（会话/路由/鉴权/端点注册表）、
                    订阅方编排 subscriber、文件传输 file_xfer、引导 bootstrap、
                    扫描聚合 devices、推流引擎 engine、接收 receiver、view 展示视图构造
                    （pub use stross_types::* 保持壳层路径兼容）
  stross-media      采集（ffmpeg 后端）、播放（PlaybackSink/cpal）、流水线 StreamConfig
  stross-bridge     平台适应桥接层：paths（数据目录）/ hostname / 平台设备枚举
                    （只产出参数注入内核，不持有状态）
  mdns              mdns-sd 0.21 的本地 fork（workspace crate；跨设备发现修复都在这里）
apps/
  stross-cli        CLI：serve/ctrl/devices/adb/push/receive/relay/scan
                    （只做参数解析+展示，流程全部调 stross-kernel 库接口）
  stross-gui        Tauri GUI（桌面 + Android 共用一套 web 前端）
  stross-relay      独立中继 CLI
scripts/            构建 / 测试 / 真机回归脚本（见 §5）
docs/               设计文档（layering-architecture.md 是分层判据；endpoint-model.md
                    是端点框架规格）
```

**分层铁律（docs/layering-architecture.md）**：线协议类型 → proto；全部服务
提供 → stross-kernel（数据面/信令/编排，单一 `Kernel` 门面）；平台适应 →
stross-bridge 与壳层；壳层只做参数解析 + 展示 + 平台适配。**内核必须平台
无关**（kernel 零路径约定、零 OS 调用、零平台分支；base_dir / hostname /
设备清单一律注入）。壳层禁止再写 HTTP 客户端、响应结构体或复制 IP 过滤规则。

## 2. 关键概念与端口约定

- **中继**：设备锚定后启动受控中继，`/ws/push` 推流端、`/ws/watch` 观看端；
  `/api/streams` `GET` 列出在线共享，`/api/info` 返回 SRT/QUIC 端口，
  `/api/peers` 返回局域网其它中继（每 15s 浏览缓存）。
- **固定端口**：中继 HTTP/WS 18777、控制面 18778（仅回环）、协商 18779、
  SRT 33462、QUIC 33464。GUI 中继端口 8777（Android 端 GUI 固定）。
- **受控中继授权**：推入流必须先建会话 + 签发一次性接入凭证（ShareToken，
  含 streamId/PIN/expiresAt/media），推流端 Hello 出示 `--share-token` 接入。
- **凭证自动协商**（免粘贴）：申请方 POST 对端 `:18779/api/negotiator/request`
  （deviceId/deviceName/media）；未知设备挂起 60s 等人工确认（GUI 弹窗；
  CLI 走 `stross ctrl negotiator-list / negotiator-respond`），已信任设备自动
  签发。协商服务桌面 GUI 与 CLI serve 都会启动。
- **mDNS 发现**：`_stross._tcp.local.`，TXT 单 key `stross` 携带整个
  DiscoveryInfo JSON；多网卡广播全部 IPv4（A 记录），浏览端按 §6 的
  选址规则挑一个可拨号地址。
- **身份**：`~/.local/share/stross/identity.json`（deviceId/name）+ 信任清单
  `trusted_devices.json`，GUI 与 CLI serve 共用同一目录。

## 3. 构建

```bash
scripts/build.sh cli             # stross-cli（debug）
scripts/build.sh android         # Android APK（需先 scripts/setup-android.sh）
# 前端 TS → JS（web/app/*.js 是编译产物，随仓库提交）：
npx tsc -p apps/stross-gui/web/tsconfig.json
```

- **Android 构建必须用 JDK 21**：系统默认 JDK 25 会让 Kotlin 1.9.25 的
  buildSrc 配置崩溃（`IllegalArgumentException: 25.0.4.1`）。
  ```bash
  export JAVA_HOME=/usr/lib/jvm/java-21-openjdk
  PATH="$JAVA_HOME/bin:$PATH" cargo tauri android build --debug -t aarch64
  # 产物: gen/android/app/build/outputs/apk/*/debug/app-*-debug.apk
  ```
- Rust 目标：`rustup target add aarch64-linux-android`（SDK 在 /opt/android-sdk，
  NDK /opt/android-ndk，均已配置）。

## 4. 测试工作流（真机）

手机经 USB 连接（adb），OPPO PLC110 常见（serial `3B6F5ME8GCL4660T`）。

```bash
./target/debug/stross adb status        # 手机型号/系统/WiFi IP/中继三端口/在线共享
./target/debug/stross adb ui-status     # 截图 + uiautomator 视图树文本
./target/debug/stross devices           # 局域网 mDNS 扫描（发现 PC + 手机）
```

**WebView 驱动（首选，比 uiautomator 流畅）**：Tauri Android 构建默认开启
WebView 远程调试（`@webview_devtools_remote_<pid>`）。脚本：

```bash
adb forward tcp:19222 localabstract:webview_devtools_remote_$(adb shell \
  "grep -o 'webview_devtools_remote_[0-9]*' /proc/net/unix | head -1" | tr -d '\r' | cut -d_ -f4)
node scripts/phone-cdp.mjs dump          # 可点击元素 id/data-act/坐标
node scripts/phone-cdp.mjs click '选择器'
node scripts/phone-cdp.mjs eval 'JS 表达式'
node scripts/phone-cdp.mjs text          # 页面可见文本
```

注意：uiautomator dump 中**滚动容器视口外的 WebView 节点 bounds 全为
[0,0][0,0]**（如设备卡片里的「共享麦克风到 TA」），tap 按文本会失败——
用 CDP 的 DOM 坐标直接点。

**端到端链路（已真机验证）**：
- 手机→PC：PC `stross ctrl create-session` + `share-token` 签发凭证 →
  手机 GUI「共享麦克风到 TA」粘贴凭证推流 → PC `stross receive` 解码。
- PC→手机：手机 GUI「接收手机麦克风」签发凭证 → PC
  `stross push --share-token <token> --stream-id <凭证streamId>
  --relay ws://<手机IP>:8777/ws/push --audio`。

## 5. 回归脚本（scripts/）

| 脚本 | 覆盖 |
|------|------|
| `quic-stale-stream-test.sh` | QUIC 硬断连（SIGKILL 推流端）→ 流 16s 内回收 |
| `srt-push-silence-cleanup-test.sh` | SRT 静默看门狗 10s + 观看端自愈 |
| `share-token-test.sh` | 受控中继凭证推流 |
| `test-frontend.mjs` | 前端无头交互（stub `__TAURI__` + 覆写 fetch） |
| `check.sh` / `check-frontend.sh` | 构建 + 单测门禁 |
| `dual-device-test.sh` / `weaknet-test.sh` / `latency-stability-test.sh` | 双机/弱网/延迟 |

## 6. 已知坑（改动前必读）

- **mDNS 多地址选址**：多网卡设备（手机 wlan0/rndis0/vgate0、PC 多网卡 +
  TUN）广播多个 IPv4，mdns-sd 的地址集合无序。浏览端 `select_reachable_ip`
  优先「与本机同 /24 网段」的 IPv4，其次任一 IPv4，最后 IPv6。**别改成
  直接取第一个**——真机随机挑中 rndis0/vgate0/USB 网卡地址导致设备卡片
  点不开（曾两次复现）。
- **Android 虚拟接口**：手机 rndis0（10.159.157.x USB 共享）、vgate0（OPPO
  游戏 VPN /32）会进 `local_ips()`；它们不是局域网可达地址。PC 端还有
  Clash TUN fake-IP（198.18.0.0/15）。过滤/选址逻辑见 crates/stross-kernel
  src/discovery.rs 与 crates/stross-transport/src/net.rs。
- **协商端点只在 serve/GUI 有**：CLI `serve` 会启动 18779；其它进程若需
  被自动协商必须自己起 `ShareNegotiator`。
- **Android 明文 HTTP**：Tauri 前端 fetch 对端 `http://ip:18779` 需 CORS
  放行（中继/协商都带 `cors_layer`，任意来源）——LAN 可信模型，不加来源限制。
- **断连检测**：quinn 默认 idle 30s 太慢（真机 force-stop 流残留半分钟）→
  服务端 idle 15s + 客户端 keepalive 10s；数据面 `PUSH_SILENCE_TIMEOUT`
  10s 兜底（rsrt 对 SIGKILL 的 UDP 对端可能永不触发）。改超时需同步改
  两个回归脚本的断言窗口。
- **receiver 运行态**：连接成功即置 running=true（不能等首帧/100ms sync，
  否则首帧早到时前端误判「流已结束」）。
- **GUI 前端**：图标用内联 SVG 雪碧图（icon()），零 emoji；`.js/.ts` 成对
  提交（`.js` 是 tsc 产物）。

## 7. 协作纪律

- 改动**不随意提交**：先用户确认；先完整验证（单测/端到端）再汇报。
- 提交方式由用户定（直接/合并/amend）；提交信息只写**相对上一提交的行为
  差异**、带 body，风格 `fix(scope): 行为差异` / `feat(scope): ...`。
- 验证偏好：新增逻辑先 `cargo test -p <crate>`；真机路径用 adb + CDP 脚本
  实测，结论记入 docs/iteration-plan.md 对应阶段。