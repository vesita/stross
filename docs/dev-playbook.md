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
  # 同步校验（uv run python -m scripts check --quick 会做）：
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
- 真源 `android/` 与 `gen/android/.../java/dev/stross/sender/` 副本**保持同步**（`uv run python -m scripts android` 复制）——
  改 Kotlin 只改 `android/`，重跑 `uv run python -m scripts android` 才进 gen/。
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
uv run python -m scripts check --quick   # fmt + clippy + 前端 tsc/同步（秒级）
uv run python -m scripts check           # 全量（再加 workspace 测试 + jsdom）
uv run python -m scripts frontend test   # 前端 jsdom 交互断言
uv run python -m scripts test-e2e discovery  # 统一发现链路回归
uv run python -m scripts test-e2e dual-node-file  # 本地双端文件互传
cargo test -p stross-kernel --lib discovery   # 发现相关单测
```

## 8. 会话中已确认/还在的坑（压缩前捞回）

- **端点 v3 已落地**（docs/framework-v3.md §3.2，唯一真源）：
  - **契约单一真源在 stross-endpoint**（内核声明、端点实现）：`Endpoint`/
    `ShareEndpoint`/`SubscribeEndpoint`/`MediaSourceEndpoint`/`SubscribeCtx`/
    `StreamConfig`/`VideoSource`/`AudioSourceConfig`/`Quality`/
    `FilePushOptions`/`impl_media_source_endpoint!` 全部随概念 crate 同仓；
  - **四能力 trait**：`StreamHost`/`FileHost`/`MediaHost`/`Runtime`（+ 组合
    `ShareHost`/`SubscribeHost`）——端点只见自己需要的能力，不再见聚合
    `EndpointApp`；`spawn_task` 在 `Runtime`（内核实现为 `tokio::spawn`）；
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
  - **订阅端点生成**：`UnifiedRegistry::generate_subscribe_endpoint`（工厂注册表，按 `EndpointClass` 分派，不再硬编码具体类）+ `Kernel::subscribe_via_endpoint`；文件订阅端 `FileReceiveEndpoint` 经 `FileHost::receive_file` 落盘（`receive_file_retry` 竞态兜底在 kernel 实现，CLI 与订阅端点共用）；
  - 改 `Endpoint` trait 时注意：所有测试 fixture（kernel/endpoint.rs、negotiator/mod.rs 的 CountingEndpoint/RecordingEndpoint）都要补 `strategy()`；`SubscribeCtx` 用 `strategy` 字段（不再是 `pick_rule`）。
- `negotiator_respond` 已改 **async**（同步 tauri 命令在 GTK 主线程调 `tokio::spawn` 无 reactor → panic）。
- **凡同步 tauri 命令可能走到 `tokio::spawn`/`tokio::time`/`tokio::net` 都必须改 async**：`endpoint_stop_share` 已改（`stop_share_by_stream` 内 `tokio::spawn` 优雅停流）。前端 invoke 对 sync/async 命令一致，无需改前端。
- **Android 无窗口级 `setFullscreen`**（`win.isFullscreen()/setFullscreen()` 均抛错）：全屏靠 CSS `.canvas-wrap.fs`（`position:fixed; inset:0`）兜底。`togglePlayerFullscreen` 必须**先应用 CSS 全屏再试 OS 全屏**，不能因 `setFullscreen` 抛错提前 return。
- **Android 播放 = 硬件 Surface 渲染**（MediaCodec→SurfaceView，后端 canvas 像素路径已弃）：前端隐藏 canvas，原生 `SurfaceView` 接管。坑位（详见 dev-notes/2026-09-01-android-surface-rendering.md）：
  - `codec.configure(fmt, surface, null, 0)` 输出到 Surface；输出 buffer 用 `releaseOutputBuffer(idx, **true**)` 渲染（false 不出画面）；
  - `SurfaceView` 用 `GONE` 起 + `SurfaceHolder.Callback`：GONE 无 surface，播放一开始**给真实尺寸**（铺满窗口）才触发 `surfaceCreated`；解码器等 surface 就绪再配置。**别用 1×1 VISIBLE 占位**——本机不触发 surface 创建（曾实测黑屏根因）；
  - **前端别把「显示 surface」门在 `decodedVideo>0`**：解码需要 surface 先存在 → 死锁（表面永不显示、解码永不启动）。按「视频链路」而非「已出画面」决定显示（订阅时记录端点 kind，视频链路缓冲期就发播放区矩形）；
  - `window.decorView` 是 `View`，`addView` 先 `as ViewGroup`；
  - 原生 Surface 置顶后**其区域内 WebView 元素不可点击**（触摸被 Surface 消费）：播放区内 hover-controls 在手机无 hover 已不可用，不构成回归；`stage-head` 头部在播放区外仍可点；
  - **全屏走原生**（Surface 铺满 + 隐藏系统栏，`set_native_fullscreen`），CSS `.canvas-wrap.fs` 只保桌面路径；
  - Android 判定「有画面」靠 `receive_links` 的 `decodedVideo` 统计（canvas 像素回调不再有帧），并要在 poll 里把有解码帧的链路设为 `activeVideoLink`（原 `onVideoFrame` 赋值在 Surface 路径失效）。
  - **原生全屏退出只能走系统返回键**（Surface 硬件 overlay 盖住 WebView 控制条，控制条不可点）：`MainActivity.onBackPressed` 全屏时**拦截为退出全屏**（恢复系统栏+恢复播放区矩形），保持 activity 存活——返回键默认 behavior 是 `finish` activity、销毁 surface → 解码器向其渲染 → `pthread_mutex_lock on destroyed mutex` 崩溃。退出后经 JNI `nativeFullscreenExited` 发事件让前端复位 `fsActive` 并重定位 surface。
- **PC 侧屏幕采集经 Wayland portal 需交互授权**：无桌面交互（自动化会话）时 portal consent 卡住 → 采集不产帧 → 端到端视频验证拿不到帧。验证 PC→手机显示需真实桌面会话授权（或改用 CLI-only 的 file 端点；file 端点无前端订阅 UI，只能命令行）。
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
- **性能诊断（第 32 轮定位，第 33 轮已修复）**：
  - **帧率 ~7fps 的根因（已纠正）**：不是手机消费瓶颈（`try_send`/`dropped=0`），也**不是
    网络/编码**（ffmpeg 仅 3.8%、在等输入），而是 **serve 侧 `bgra_to_yuv420p_scaled`（纯 Rust
    双线性，debug 未优化 ~85ms/帧）**。取证：PC 看自己中继（loopback）也只有 ~5.6fps（排除网络）；
    serve 85.9% vs ffmpeg 3.8%。
  - **修复（第 33 轮）**：**缩放交给 ffmpeg swscale**——喂 ffmpeg 原生 BGRA +
    `-vf scale=WxH,format=yuv420p`。serve 只做 stride 规整拷贝。为此需**先探测原生尺寸再起
    ffmpeg**（`-video_size` 必须一致），故 `wayland::start` 改为两段式（回传尺寸→等 stdin），
    新增异步 `StreamSession::spawn_wayland`，`CaptureBackend::start` 改 `#[async_trait]` async。
    实测：serve 85.9%→**3.5%**，PC 源 5.6→**~23fps**，ffmpeg 61%（成新瓶颈），手机显示 ~11fps。
  - **下一杠杆**：PC 上 30fps 需 **GPU/VAAPI 编码**（encode 现在才是瓶颈）；手机显示 ~11fps 是
    独立的 Android `receive-frame` base64→WebView 路径（二进制 Channel + 重打 APK 是后续）。
  - **测试方法坑**：手动 CDP 调 `startReceiveLink()` 只起手机本地接收器，**不触发 PC
    `on_subscribed`→`share`→push**（端点 idle、中继报「流不存在」）。必须走**真实订阅按钮**
    （`subscribe-endpoint` + `#sub-confirm-btn`）。`[data-endpoint="screen:0"]` 的冒号要加引号。
- **UI 状态机（FSM）响应式架构**：
  - `web/app/state.ts` 集中维护 `uiFSM` 状态与 `dispatchUIAction` 派发中心；
  - 应用阶段 `AppStage`（`idle`/`managing`/`streaming`/`error`）与播放器模式 `PlayerDisplayMode`（`empty`/`buffering`/`videoOnly`/`audioOnly`/`audioVisualMix`）严格由链路活跃度与视图模式转移；
  - `ui.ts` 经 `subscribeUIFSM` 订阅全量状态转移并响应式同步 DOM（禁止在业务分支散落 ad-hoc 的 DOM 显隐操作）。
- **Android 原生播放器最佳实践与健壮性**：
  - **常亮管理**：`PlaybackPlugin.kt` 在 `startPlayback` 时通过 `activity.runOnUiThread` 设置 `FLAG_KEEP_SCREEN_ON`，`stopPlayback` 时重置；
  - **音频焦点与 Ducking**：`AudioManager` & `AudioFocusRequest`（API 26+）监听焦点变化，电话/通知打断时音量自动 Duck 至 25%，焦点恢复自动拉回 100%；
  - **解码器安全降级**：`createVideoDecoder` 首选 `low-latency=1` 与 `priority=0` 配置，若设备厂商驱动抛异常，自动回退标准 `MediaFormat` 配置，杜绝启动崩溃；
  - **全屏智能自适应旋转**：前端在全屏时根据视频真实宽高比调用 `set_screen_orientation`，横屏内容自动旋转 `SENSOR_LANDSCAPE`，竖屏内容保持 `SENSOR_PORTRAIT`，退出全屏恢复 `UNSPECIFIED`。
- Android 屏幕端点（MediaProjection FGS）、摄像头（CameraX）、剪贴板（E 阶段）待扩充。
- 协议优化排队：watch 鉴权 + stream_id 不可枚举、应用层保活控制帧、pts 回绕。

## 9. 代码质量与整洁纪律（零 dead_code 规范与技术债收敛）

- **严格零 `#[allow(dead_code)]`**：无用代码必须就地删除，绝不允许使用 `#[allow(dead_code)]` 掩盖死代码。
- **RAII / 生命周期守护字段命名规范**：仅持有用于维持生命周期或 Drop 触发的结构体字段（如采集任务控制器、发送通道存活守卫），统一使用 `_` 开头命名（如 `_wayland`、`_tx`）。Rust 编译器原生支持下划线前缀表示故意保留的 RAII 字段，无需且不得标记 `#[allow(dead_code)]`。
- **精确平台条件编译 `#[cfg(...)]`**：平台特定函数/类型（如 Linux 独有的 ufw/pkexec 防火墙放行逻辑、webrtc candidate mDNS 解析）必须标注精确的目标平台与测试属性（如 `#[cfg(any(feature = "discovery", test))]`），严禁使用宽泛的 `allow(dead_code)` 压制非当前平台的未调用告警。
- **同构分支与模式合并**：处理通道断开或异常时，相同行为的 match 分支必须使用 `|` 语法合并（如 `Ok(None) | Err(_) => break`、`Ok(Some(SessionPacket::Media(_) | SessionPacket::Control(_))) => {}`），消除冗余分支与认知开销。
- **零冗余拷贝（Zero Redundant Clone）**：已拥有所有权（owned）的字段映射直接移交所有权（如 `.map(|m| m.name)` 代替 `.map(|m| m.name.clone())`）；只读消费的函数入参统一传入引用切片（`&str` 或 `&T`）而非接受 `String`/`T` 后在内部再借用，杜绝调用端产生非必要的 `.clone()`。
- **测试并发与时序防抖**：跨线程测试（如解码器/写线程后台异步消费）断言丢帧或调度时，避免单次 try_send / 单次 push 竞态，采用有界循环确保可靠触发；对 ffmpeg 预热等重型测试的超时窗口设置合理裕量，杜绝 CI 与高并发压力测试下的假性红灯。
