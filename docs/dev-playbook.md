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

- **端点 v2.1（分享/订阅独立契约 + 能力族）已落地**（docs/endpoint-model-v2.md §3）：
  - **契约单一真源在 stross-types/contract.rs**（内核声明、端点实现）：`Endpoint`/
    `ShareEndpoint`/`SubscribeEndpoint`/`MediaSourceEndpoint`/`EndpointApp`/
    `SubscribeCtx`/`StreamConfig`/`VideoSource`/`AudioSourceConfig`/`Quality`/
    `FilePushOptions`/`impl_media_source_endpoint!` 全部上移；stross-endpoint 的
    `contract.rs` 只是 `pub use stross_types::contract::*` shim，pipeline/file
    重导出数据契约（路径兼容）。**改契约去 stross-types，别在 endpoint 里加**；
  - `EndpointApp` 新增 `spawn_task`（内核实现为 `tokio::spawn`）——契约层零
    tokio 依赖，端点 fire-and-forget 一律经它；
  - **契约拆分**：`Endpoint`（公共视图：身份/kind/class/策略）→ `ShareEndpoint`
    （load/available/share）+ `SubscribeEndpoint`（subscribe）——**没有双向占位**；
    注册表持 `Box<dyn ShareEndpoint>`，订阅端点生成返回 `Box<dyn SubscribeEndpoint>`；
  - **文件结构**：`stross-endpoint/src/share/`（screen/audio/file）与
    `stross-endpoint/src/subscribe/`（media/file）分目录；公共 API 路径不变
    （`stross_endpoint::ScreenEndpoint` 等仍从 crate 根导出）；
  - **能力族**：`EndpointClass`（Graph/Audio/File/Clipboard/Input/Service，按 kind
    推导）；`MediaSourceEndpoint` 族实现分享端（`impl_media_source_endpoint!` 宏
    生成 Endpoint+ShareEndpoint 样板）；`generate_subscribe_endpoint` 按族分发
    （File→落盘，Graph/Audio→播放器 `MediaReceiveEndpoint`）；
  - **序列化=内核数据契约**：`pick::Loader` 携带 `SerializeRule`（`loader_for`
    工厂）；协商 `checked_strategy` 与 `receive_media` 对未实现规则（Chunked）
    直接拒绝，不静默降级；
  - **Android 屏幕端点**已适配（`share/screen/android.rs` 探测恒可用；采集执行
    在壳层 `AndroidCapture`；真机测试后置）；
  - 改 `Endpoint` 时注意：测试 fixture（kernel/endpoint.rs、negotiator/mod.rs）
    要按拆分写 `impl Endpoint` + `impl ShareEndpoint` 两段；宏 `impl_media_source_endpoint!`
    用 `$t {{ ... }}, {{ ... }}` 花括号 item 序列形态（tt/item 片段会二义性）。
- **端点框架 v2（三层注册表 + 双特性）已落地**（docs/endpoint-model-v2.md）：
  - `UnifiedRegistry`（kernel/endpoint.rs）= 本机 `EndpointRegistry`（行为对象）+ 互联节点表（目录拉取映射）；订阅统一 `resolve_strategy(node_id, endpoint_id, strategy_id)` 查表，本机走 `strategy()` 单一真源、远端走目录映射；
  - **策略**：`EndpointStrategy { strategy_id, serialize, pick }`，端点 `strategy()` 组合方法（替代 v1 `pick_rule()`）；平铺 `transport_profile`/`pick_rule` 保留为默认策略协商摘要（旧对端兼容，勿删）；
  - **订阅端点生成**：`UnifiedRegistry::generate_subscribe_endpoint` + `Kernel::subscribe_via_endpoint`；文件订阅端 `FileReceiveEndpoint` 经 `EndpointApp::receive_file` 落盘（`receive_file_retry` 竞态兜底在 kernel 实现，CLI 与订阅端点共用）；
  - 改 `Endpoint` trait 时注意：所有测试 fixture（kernel/endpoint.rs、negotiator/mod.rs 的 CountingEndpoint/RecordingEndpoint）都要补 `strategy()`；`SubscribeCtx` 用 `strategy` 字段（不再是 `pick_rule`）。
- `negotiator_respond` 已改 **async**（同步 tauri 命令在 GTK 主线程调 `tokio::spawn` 无 reactor → panic）。
- **凡同步 tauri 命令可能走到 `tokio::spawn`/`tokio::time`/`tokio::net` 都必须改 async**：`endpoint_stop_share` 已改（`stop_share_by_stream` 内 `tokio::spawn` 优雅停流）。前端 invoke 对 sync/async 命令一致，无需改前端。
- **Android 无窗口级 `setFullscreen`**（`win.isFullscreen()/setFullscreen()` 均抛错）：全屏靠 CSS `.canvas-wrap.fs`（`position:fixed; inset:0`）兜底。`togglePlayerFullscreen` 必须**先应用 CSS 全屏再试 OS 全屏**，不能因 `setFullscreen` 抛错提前 return。
- 发现「陈旧条目」未清：设备死后手机列表可能仍显示（依赖 mDNS TTL；未做主动超时重探）。**已部分修**：前端 `refreshDevices` 剔 `!d.online`（探测失败即移除），手动地址仍保留；mDNS TTL 窗口内的死节点因此不再长期残留。
- **推流引擎已并发化**：`kernel.engine`（`Option<RunningStream>`）→ `engines: HashMap<stream_id, RunningStream>`。端点模型允许任意端点并发推（屏幕+系统声音等），不再有「已经在推流中」单流限制；仅同 stream_id 重复才拒。回归测试 `concurrent_streams_both_start`。
- **接收端多链路已落地**（通信模式 v2 Phase C）：`Kernel::receivers: HashMap<link_id, Receiver>`——`start_receive_link`/`stop_receive_link`/`receive_links`/`take_receive_frames_for`；桌面 GUI 右栏「接收」逐条链路显示 + 独立停止，画布显示最近活跃视频链路，纯音频链只出声。**旧单流 API（`start_receive`/`stop_receive`/`receive_status`/`take_receive_frames`）落预留槽 `main` 兼容**（Android 单链播放路径不变）。改接收侧先看这两套 API 的分工。
- **QUIC 连接复用（Phase C）坑位**（docs/comm-mode-v2.md §5 附）：
  - **确认门**：`QuicMediaSession` 在 StreamOpened（Welcome/Ready）送达前不吐媒体帧——否则等确认的循环（`connect_watch`）会吞掉先到的首关键帧（负载下必现超时）；
  - **recv 事件优先**：已确认后 `biased` 事件分支优先（与旧 `QuicDataSession` 一致）；
  - **FIFO 配对禁止 `(a.pop(), b.pop())` 元组**：元组先求值两个 pop，单侧空时另一侧被提前消费丢弃——先判空再 pop；
  - 客户端**链路管理器** `QUIC_LINKS`（stross-transport 进程级静态，按 (host,port) 复用）：上层 `RelayClient`/`connect_watch` 零改动自动共享连接；中继 peer 循环（data_plane.rs `quic_peer_loop`）把 control OpenStream ↔ accept_bi FIFO 配对，`[连接][stream_id]` demux；
  - 紧凑帧头 `Frame2`（14 字节，codec 移 OpenStream 协商）：**仅 QUIC 复用连接**用；WS/SRT 单流路径保留 v1 24 字节头；
  - 语义 id 派生 `derive_stream_id(endpoint_id, transport_profile, pick_rule)`：端点订阅 grant 流 id 已改派生 id（不再 sess-N），会话幂等 `ensure_session_with_id`；订阅方一致性校验不一致仅告警；
  - 中继 peer 循环需直接驱动 quinn 流类型 → **stross-kernel 有 `quinn` 直接依赖**（传输层仍属主）。
- **播放侧 PTS 调度（pacer）**：
  - 过水位丢队尾延迟控制器 `drop_over_watermark` **已接线**进 `pacer_loop`（ffmpeg.rs）——此前是死代码（仅 schedule.rs 单测调用），`paced_dropped` 恒 0；改 pacer 循环时**别删那次调用**，防回退测试 `pacer_loop_wires_watermark_drop`（合成帧直驱，不经解码子进程）；
  - 既有 flaky 测试 `video_pacing_holds_burst_and_emits_on_schedule`：**首帧窗口 2s**（整机高负载下 ffmpeg 子进程启动+首帧解码可能超 800ms，全 workspace 并行偶发红），首帧后收紧 800ms 判帧间节奏——别把首帧窗口改回 800ms；若该测试仍偶发先隔离重跑确认，勿误判为播放器回归。
- **播放显示管线（「解码帧率高、播放帧率低」根因，第三十轮已修）**：
  - **桌面帧传输走 tauri `Channel<Vec<u8>>` 二进制**（`receive.rs::pack_frame` 16 字节头 `STRF+w+h+pts` + RGBA，前端 DataView 解析零拷贝）——**别改回 base64+JSON 事件**（1.5MB/帧字符串跨进程 IPC + 前端 atob/逐字节拷贝 = 瓶颈）；`on_frame` 是**非 Option** 的 `Channel` 参数（`Option<Channel>` 走通用 CommandArg 要求 Deserialize，编译不过）；
  - **前端 RAF 节流**：帧回调（Channel/事件）只存 `pendingFrame` 最新帧，`requestAnimationFrame` 里画（丢中间帧、不积压）；**禁止在帧回调里调 `renderRecvLinks()`**（每帧 DOM 重建是主线程大开销，第三十轮移除）；
  - **Android 显示仍走 `receive-frame` base64 事件**（`mobile_jni.rs` Kotlin 解码 → JNI → Rust 事件），前端双路径（`ensureRecvFrameListener` + `newFrameChannel`）——改一边别忘了另一边；
  - `rgba_scaled`（stross-endpoint convert/rgba.rs）是 **12 位定点双线性**（热路径 720p→720×405 ≈ 1ms/帧，改回浮点慢 ~6×）；
  - **接收面板帧率统计**（第三十一轮）：链路行「解码 ~Nfps · 显示 ~Nfps」= poll 差分按实际间隔归一化 + **最近 4 次滑动平均**——瞬时差分 0（流暂停的 poll 间隙）会抹掉历史，别改回瞬时差分；「解码高、显示低」即显示管线瓶颈的直观信号。
- 名称不一致：mDNS `pico`(hostname) vs `/api/discovery` `Stross 设备`(identity.device_name)。
- **PC 侧要「分享」不必起 GUI**：`serve` 已 `seed_platform_endpoints`（三件套注册、默认未通告），
  用控制面公开即可让手机订阅：
  `stross ctrl endpoint publish --device screen:0 --visibility public --delivery pull`。
  （`serve` 不起控制面端口时 `stross ctrl` 连 `ws://127.0.0.1:18778/ws/ctrl` 需 serve 在跑。）
- **手机端订阅按钮 bounds=[0,0]（视口外/折叠卡片）**：`phone-cdp.mjs click` 会因 `visible=false` 拒点，
  必须用 `eval` 直接 `.click()`：`document.querySelector('[data-act="subscribe-endpoint"][data-endpoint="screen:0"]').click()`。
  且订阅后面板切到「消费播放台」，设备列表重渲染会把远端目录从 DOM 移除——再订下一条前要重新
  `call('loadRemoteDir', ...)`。
- **性能诊断（第 32 轮实测，未改码）**：
  - **帧率 ~7fps 不是手机消费瓶颈（初判已修正）**：Android 接收走 `kernel::receiver::receive_raw_loop`，
    消费通道是 `try_send`（满即丢、不反压）+ `dropped` 计数；实测 `dropped=0` 且 `received≈7fps`
    → 手机端 base64 `feedVideo` 不背压、不是瓶颈。~7fps 是**源产帧率**（静止屏伤害驱动 + 
    `recv_frame_timeout(30ms)` 阻塞 + `interval` 节流 → 采集循环只给 ffmpeg ~7fps）。
  - **判定前提**：要确认动态屏下传输/编码上限，需动态画面（播放视频/高频刷新终端）复测
    「手机端 received/显示」能否到 ~30fps——此为帧率瓶颈是否真成立的判定前置。
  - **移动端 `feedVideo` 仍走 base64+JSON 事件**：与桌面 `Channel<Vec<u8>>` 不对称。这是**降低 IPC/前端
    开销的基建优化**（对动态屏高帧率更稳），但**不会提升静止屏帧率**，别当帧率修复做。
  - **高功耗**：静止屏下 `stross serve` 仍 ~45–65% CPU（`top -H` 见一根 hot tokio worker ~78%）。
    `wayland.rs` 采集循环无视屏幕变化、按 `interval` 持续把上一帧送 ffmpeg 编码；另 `recv_frame_timeout`
    是阻塞 std mpsc 在 tokio worker 上（疑即 78% 来源，改非阻塞 poll 或挪 blocking 线程比改节流更直接）。
    pipewire 日志偶见 `SPA_CHUNK_FLAG_CORRUPTED`。
  - **Android 音频待复测**：订阅系统声音后手机 `feedAudio` 大量上报但 `audioBlocks/audioBlocksIn` 恒 0；
    logcat 反复 `AudioTrackShared: Track invalidated` + `writeFramesHelper getNextBuffer failed -11`；
    且（08-30）`PlaybackPlugin.startAudioTrack` 的 `AudioTrack.write` 有 native crash 栈。疑似 WIP 新增
    低延迟 AudioTrack（`FLAG_LOW_LATENCY` + `PERFORMANCE_MODE_LOW_LATENCY` + `minBuf*2`）在这台设备
    兼容问题，需稳定音频源复测确认是否新回归（本轮 PC 系统声音源静默，未定论）。
- Android 屏幕端点（MediaProjection FGS）、摄像头（CameraX）、剪贴板（E 阶段）待扩充。
- 协议优化排队：watch 鉴权 + stream_id 不可枚举、应用层保活控制帧、pts 回绕。
