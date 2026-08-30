# 开发速查卡（AI 恢复上下文用）

> 目的：对话压缩后，让新 AI **直接读本文** 即可获知本项目「套路」，
> 不用再花大量精力重新摸索。**本文件是人工经验 + 会话踩坑的浓缩**，
> 优先于从零推断。技术细节（端口/分层/验证偏好）以 AGENTS.md 为准，此处
> 只补**会话里刚踩过、AGENTS.md 没细写、且易反复踩**的套路。

> **开发策略（用户拍板）**：**允许破坏性更新**——协议/架构可 breaking，
> 全端同步演进，不做新旧 wire 兼容层。所以改帧头/协议不用背兼容包袱。
> **通信模式 v2**（控制面协商 + 数据面按 id 复用 + 传输/解读模块）是当前演进方向，
> 见 docs/comm-mode-v2.md（Phase A 端点档案 → Phase B 数据衔接层 → Phase C 流级复用）。
> **文档纪律**：新文档挂 docs/README.md 清单；单一真源；术语「共享/订阅」。

---

## 1. 构建时序坑（最重要，反复踩）

- **Tauri `frontendDist=../web` 在编译期嵌入**桌面 / Android 二进制。
- **增量 `cargo build -p stross-gui` 不会因 `web/*.js` 变化而重新嵌入资产**
  （tauri asset codegen 按内容 hash 缓存，增量不重跑）。现象：改了前端文案，
  `cargo run -p stross-gui` 仍是旧 UI。
- **改前端后必须 clean 重build（二选一）**：
  ```bash
  cargo clean -p stross-gui && cargo build -p stross-gui --bin stross-gui   # 桌面
  cargo clean -p stross-kernel 2>/dev/null; cargo tauri android build --debug -t aarch64  # Android
  ```
  或 `touch apps/stross-gui/src-tauri/tauri.conf.json` 强制 build.rs 重嵌入。
- 同理改 **kernel/发现的 Rust 代码**后，Android 端（手机 APK）也需重build 才生效——
  曾因增量未重编译 kernel 导致手机端逻辑滞后（如 dedup 未生效）。
- **验证嵌入是否生效**：解压生成的资产再搜文案（tauri 资产是 **brotli 压缩**，不能直接 grep 二进制）：
  ```bash
  ND=$(ls -dt target/debug/build/stross-gui-* | head -1)
  for f in "$ND"/out/tauri-codegen-assets/*.js; do brotli -d -c "$f" 2>/dev/null; done | grep -c "已共享"
  ```

## 2. 前端（Tauri web，桌面+Android 共用一套）

- 唯一前端目录 `apps/stross-gui/web`，Tauri 桌面与 Android **共用**（无需分端改）。
- `.ts` 是真源，`app/*.js` 是 tsc 产物**提交进仓库**；改 `.ts` 后必须生成 `.js` 并同步：
  ```bash
  npx -y -p typescript@5.9.3 tsc -p apps/stross-gui/web/tsconfig.json --pretty false   # 生成
  # 同步校验（check.sh --quick 会做）：
  npx -y -p typescript@5.9.3 tsc -p apps/stross-gui/web/tsconfig.json --pretty false --outDir /tmp/x
  cmp -s /tmp/x/endpoints.js apps/stross-gui/web/app/endpoints.js
  ```
- **前端交互模型定稿**（用户拍板，改文案/弹窗先对照）：
  - **共享** = 我是内容源（推送预备），把端点共享出去；
  - **订阅** = 我是接收方；握手完成后共享端向订阅者推送；
  - **方向（pull/push）是系统/端点决策，两侧都不选**——**共享弹窗也不再有「数据面方向」字段**，
    `endpoint_publish` 传 `endpoint.delivery || 'pull'`（系统决定）；
    `Push-only` 端点由框架按宣告自动预置本机接收。
  - 术语用**「共享」**（不用「通告/广播」——「广播」与 mDNS 广播/旧版「广播共享」歧义）。
  - 节点卡片**不展示**「中继/共享/接收」角色字眼；端点行 meta 不展示「实时」这类抽象类别；
    徽标只展示「已共享 · 可见性」、**不含方向**。
- **PC 桌面布局（重做后）**：`设备`= 窄侧栏（发现/共享）+ `接收`= 主视区（订阅流播放）。
  接收面板有**空状态**（图标 + 「尚未接收共享」），无空荡占位。窄屏（`@media ≤860px`）折叠单栏（安卓同款）。
- **文案无技术用语**：不接受 `端点/目录（设备→端点）/中继端口/传输=/数据面/endpoint_ls/协商端口缺省/通告`
  等词出现在用户可见文本（技术词只在代码注释）。共享弹窗只保留「谁可以订阅」；目录标题「可订阅的内容」。

## 3. Android / Kotlin 分层结论（用户反复问「Kotlin 还有用吗」）

- **Kotlin 不是死代码**。`android/{MediaPlugin,PlaybackPlugin,ProjectionService,MainActivity}.kt`
  全部服务于 **Android 采集/播放**，只在 `cfg(mobile)` 下编译（桌面 `cfg(not(mobile))` 不碰）。
- 6 个 Kotlin `@Command`（startCapture/stopCapture/startPlayback/stopPlayback/feedAudio/feedVideo）
  全被 `src/mobile.rs` 经 `run_mobile_plugin` 调用。`ProjectionService` 被 `MediaPlugin` 引用。
- 真源 `android/` 与 `gen/android/.../java/dev/stross/sender/` 副本**保持同步**（setup-android.sh 复制）——
  改 Kotlin 只改 `android/`，重跑 `scripts/setup-android.sh` 才进 gen/。
- `mobile_jni.rs`（Kotlin⇄Rust JNI 直传）仅 `cfg(all(mobile,target_os="android"))` 编译。

## 4. `scan_lan` 发现聚合（kernel discovery/aggregate.rs）

- 逻辑：`Discovery::browse`(mDNS) → `scan()` **(按 instance 名去重**，IPv4 优先) → 若 **mDNS 零远端**
  则触发 **子网单播扫描回退**（`subnet_scan`，纯单播探测 18779 `/api/discovery`）→ 手动地址并入。
- **去重关键**：合并结果**按 `ip:port` 去重**（`seen: HashSet`）。曾漏掉子网扫描与 mDNS 双路径去重，
  导致手机界面出现**两张相同节点卡片**。`scan_lan` 现在把 mDNS 结果先建 `seen`，子网/手动地址都过 `seen`。
- **可被发现（discoverable）门控**：`Kernel::discovery_manifest()` 在 `discoverable==false` 时返回 `None`
  → `/api/discovery` 404 → 子网扫描也扫不到。**「关闭 = 所有发现路径不可见」**（隐私优先，用户拍板）。
  曾出现 bug：关了「可被发现」仍被子网扫描发现（因为开关只停 mDNS 广播，没门控 /api/discovery）。

## 5. 发现/网络事实（本机环境）

- 手机 OPPO PLC110（`adb devices` serial 以输出为准），包 `dev.stross.sender`，GUI 中继 **8777**。
- 手机接口：`wlan0=192.168.11.60/24`（WiFi）、`rndis0=10.159.157.104/24`（USB 共享）、`vgate0=172.30.242.158/32`（OPPO 游戏 VPN）。
- PC：`enp6s0=192.168.11.61/24`、`Mihomo(Clash TUN)=198.18.0.1/30`（fake-IP，**进 local_ips 会污染**，须剔除）。
- **本 WiFi AP 拦「下行多播」**（Su 路由）：手机收不到下行多播 → **mDNS 跨设备不可靠**；
  但**单播双向通** → 子网扫描回退可用。**USB 共享网络（10.159.157.0/24）多播通畅**，可作稳定发现路径。
- 「可被发现」开关**默认关**；CLI `serve --discoverable` 显式开启；GUI 经 `settings.json` 持久化。

## 6. 真机验证套路（adb + CDP）

```bash
# 重装 APK 后重启 app + 重新 forward（webview pid 会变）：
PID=$(adb shell pidof dev.stross.sender | tr -d '\r')
adb forward --remove tcp:19222; adb forward tcp:19222 localabstract:webview_devtools_remote_${PID}
# 读取/操作手机 UI（CDP，比 uiautomator 流畅）：
node scripts/phone-cdp.mjs text      # 可见文本
node scripts/phone-cdp.mjs dump     # 可点击元素 id/data-act/坐标
node scripts/phone-cdp.mjs click '#scan-btn' | '.dev-card.xxx .dev-head' | '[data-act="subscribe-endpoint"]'
node scripts/phone-cdp.mjs eval "<js>"
# 手机 Rust 日志走 logcat（tag=RustStdoutStderr）：
adb logcat -d 2>/dev/null | grep -iE "stross_kernel::discovery::aggregate|触发子网|发现.*台"
# 截图（Wayland 桌面难截；用 adb）+ 手机 logcat 是主力验证通道
```

## 7. 常用门禁 / 验证

```bash
bash scripts/check.sh --quick   # fmt + clippy + 前端 tsc/同步（秒级）
bash scripts/check.sh           # 全量（再加 workspace 测试 + jsdom）
node scripts/test-frontend.mjs  # 前端 jsdom 交互断言
bash scripts/discovery-test.sh  # 统一发现链路回归
bash scripts/dual-node-file-test.sh  # 本地双端文件互传
cargo test -p stross-kernel --lib discovery   # 发现相关单测
```

## 8. 会话中已确认/还在的坑（压缩前捞回）

- `negotiator_respond` 已改 **async**（同步 tauri 命令在 GTK 主线程调 `tokio::spawn` 无 reactor → panic）。
- **凡同步 tauri 命令可能走到 `tokio::spawn`/`tokio::time`/`tokio::net` 都必须改 async**：`endpoint_stop_share` 已改（`stop_share_by_stream` 内 `tokio::spawn` 优雅停流）。前端 invoke 对 sync/async 命令一致，无需改前端。
- **Android 无窗口级 `setFullscreen`**（`win.isFullscreen()/setFullscreen()` 均抛错）：全屏靠 CSS `.canvas-wrap.fs`（`position:fixed; inset:0`）兜底。`togglePlayerFullscreen` 必须**先应用 CSS 全屏再试 OS 全屏**，不能因 `setFullscreen` 抛错提前 return。
- 发现「陈旧条目」未清：设备死后手机列表可能仍显示（依赖 mDNS TTL；未做主动超时重探）。**已部分修**：前端 `refreshDevices` 剔 `!d.online`（探测失败即移除），手动地址仍保留；mDNS TTL 窗口内的死节点因此不再长期残留。
- **推流引擎已并发化**：`kernel.engine`（`Option<RunningStream>`）→ `engines: HashMap<stream_id, RunningStream>`。端点模型允许任意端点并发推（屏幕+系统声音等），不再有「已经在推流中」单流限制；仅同 stream_id 重复才拒。回归测试 `concurrent_streams_both_start`。
  - **接收端仍单流**：手机/PC 的「接收」面板一次只播放一条流（`start_receive` 切换流会停旧流）。所以 PC 并发推了 screen+audio 两条流，订阅方只看到最后订阅的那条——「屏幕+声音同屏播放」需要**链接复用**（把多路媒体并进一条流 / 运行中加轨），属后续想法。
- 名称不一致：mDNS `pico`(hostname) vs `/api/discovery` `Stross 设备`(identity.device_name)。
- Android 屏幕端点（MediaProjection FGS）、摄像头（CameraX）、剪贴板（E 阶段）待扩充。
- 协议优化排队：watch 鉴权 + stream_id 不可枚举、应用层保活控制帧、pts 回绕。
