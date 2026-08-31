> 2026-08-31。现象：手机订阅 PC 屏幕，画面帧率只有 ~5–7fps。用户直觉是
> 「数据传输（传输）瓶颈」。**结论：根本不在传输层，而在 serve 端的图像缩放。**

## 结论一句话

**PC→手机 PC 端整个链路（采集→编码→中继→网络→解码显示）里，唯一卡住的是
`serve` 进程里的 Rust 双线性缩放**（`bgra_to_yuv420p_scaled`，1080p→720p）。
网络、手机解码、ffmpeg 编码都不是瓶颈。

## 关键证据（怎么一步步排除的）

1. **手机侧两点分开测**：前端 `frames`（Kotlin 解码→事件）≈ 5.33fps；
   Rust 侧 `received`（从中继收到的帧）≈ **5.2fps**，`dropped=0`。
   => 帧到达手机就只有 5fps，不是手机解码跟不上（不丢帧、也不是解码慢）。

2. **PC 看自己的中继（loopback）**：`stross receive --relay ws://127.0.0.1:18777`
   同样只有 ~5.6fps（5s 收 28 帧 / 解码 25）。=> **排除 wifi/跨机网络**。
   （这一步是分水岭：手机 5.2 和 PC loopback 5.6 一致，说明瓶颈在共享源侧。）

3. **看 CPU**：`serve` **85.9%**；ffmpeg 子进程只有 **3.8%**（空闲在等输入）。
   `ffmpeg ... -c:v libx264 -preset veryfast ...` 3.8% => 编码**不是**瓶颈，
   是 serve 没喂够帧（ffmpeg 在等）。
   => serve 内的**逐帧转换**（唯一的重活）才是瓶颈，且它在跑（85% 说明内容在变化）。

## 为什么是「内容变化」才触发

`wayland.rs` 采集循环（此前已加 `pixel_hash` 内容指纹 + `with_framerate(fps)`）：
- **静止帧**：指纹匹配 → 跳过缩放 → 复用缓存帧 → 仍按 30fps 喂 ffmpeg（保 PTS）。
- **内容一直变**（桌面有动图/视频/动画）：每帧指纹都不同 → **每帧都做
  `1080p→720p` 双线性缩放**（纯 Rust、逐像素 f32、还每帧分配 3.7MB 的
  `scaled` 缓冲）。单线程约 **100–150ms/帧** => 输出被压到 ~5–6fps。

`next_write = now + interval` 里 `now` 取在转换之后 => 转换耗时直接叠进周期 =>
超过 33ms 就出不了 30fps。这是「转换慢 → 帧率被压」的直接机制。

## 修复方向（已实现并实测）

**根因**：瓶颈是 serve 侧 `bgra_to_yuv420p_scaled`（纯 Rust 双线性，未优化 debug 下
~85ms/帧），不是网络/手机/编码（编码仅 3.8%，在饿着等输入）。

**方案（把缩放交给 ffmpeg swscale）**：
- `args.rs::wayland_rawvideo_command(cfg, native_w, native_h)`：ffmpeg 以**原生分辨率
  BGRA** 输入 + `-vf scale=WxH,format=yuv420p`（swscale）。
- `wayland.rs::feed_loop`：只按 stride 规整拷贝原生 BGRA（memcpy 级，`stride==w*4` 时
  整块拷贝），不再缩放/转格式/哈希。
- **时序重构**：ffmpeg 的 `-video_size` 必须等于原生尺寸，而原生尺寸要等 portal 回报。
  原架构先起 ffmpeg（尺寸未知）→ 改为 `spawn_wayland（异步）`：采集任务先探测原生尺寸
  → oneshot 回传 → 管线据此起 ffmpeg → 再把 stdin 经 oneshot 送回采集任务喂帧。
  `CaptureBackend::start` 相应改为 `async`（`async_trait`）。

**实测（Android 手机订阅 PC 屏幕，内容在动）**：

| 指标 | 修复前 | 修复后 |
|------|--------|--------|
| serve CPU | 85.9% | **3.5%** |
| ffmpeg CPU | 3.8%（闲等） | 61.4%（swscale+编码，成了新瓶颈） |
| PC 源帧率（`received`）| ~5.6 fps | **~23 fps** |
| 手机显示 fps | 5.3 | **~11 fps**（仍受手机解码→base64→WebView 限） |

结论：**PC 源瓶颈已解决（5.6→23fps，serve CPU 85.9→3.5%）**，瓶颈移到 ffmpeg 编码
（61%，未满）。要上 30fps：GPU/VAAPI 编码是下一杠杆；手机显示仍卡在 ~11fps（那是
独立的 Android 端 `receive-frame` base64→WebView 路径，需二进制 Channel + 重打 APK）。

## 反向方向帧率（手机共享屏幕 → PC 订阅）— 已验证

手机端 MediaProjection 采集 + MediaCodec 编码，PC `stross endpoint subscribe` 订阅，
再另起 `stross receive` 测收流：**6s 收 180 帧 / 解码 176 帧 ≈ 30fps**。
=> 手机→PC 方向**不是瓶颈**（媒体编码全硬件，MediaProjection 采集 30fps 无压力）。
手机 CPU 162% 高，是因为本次同时在做「回放 pico 屏幕 + 采集自身屏幕」的双向负载。

**驱动手机反向共享屏幕的路径**：`stross endpoint subscribe --host <手机IP> --port 18779
--endpoint screen:0`。走协商端点（18779）触发手机 `on_subscribed`→`AndroidCapture::start`
→ Kotlin `startCapture`（MediaProjection `createScreenCaptureIntent`）。**MediaProjection
授权已获得**（否则 encoder 不会跑、PC 也收不到帧）。

**「需确认」标签误导**：手机 UI「本机设备」屏幕行显示「已共享 · 需确认」，但
capture 实际在跑（logcat `c2.mtk.avc.encoder` 30fps）。可能是 UI 状态文案与真实
capture 状态不同步（跟进：是否需把「需确认」做成真实等待授权的展示态）。

## 排查过程中的坑（顺手记）

- **别用 `pkill -f 'stross serve'`**：会匹配到包着它的 bash 命令而自杀。
  用 `kill <pid>`。
- **`perf top -p <serve_pid>` 在本机拿不到输出**（无样本/权限）——别在这上面耗，
  直接看 `ps pcpu` 解耦 serve 与 ffmpeg 两个进程的 CPU 即可定位。
- **「手机没在收」的假象**：前端展示「暂无活跃的串流」但中继 `watchers:1`。
  查实态别只看 UI 文案，用 CDP 读 `recvLinks.size / recvLinks.get(...).frames`
  和 Rust 的 `call("receive_links")`（`received / decodedVideo / dropped`）。
- **手动 CDP 调 `startReceiveLink()` 只启动手机本地接收器**：**不会触发 PC 的
  `on_subscribed`→`share`→push**（端点停在 idle，`推流 未运行`，中继报「流不存在」）。
  必须走**真实订阅按钮**（`subscribe-endpoint` + `#sub-confirm-btn`），经协商端点触发
  PC 开始共享。
- **`[data-endpoint="screen:0"]` 选择器里的冒号必须加引号**，否则 `querySelector`
  抛 SyntaxError。
