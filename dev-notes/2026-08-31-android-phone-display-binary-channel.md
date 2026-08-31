# Android 手机端播放显示链路改造 — 二进制 Channel + 原始 RGBA

> 2026-08-31。目标：手机端（PC→手机订阅播放）显示帧率从 ~11fps 提上去 + 全屏高清
> （上限 1080p，`MAX_FRAME_W=480 → 1920`）。**「压缩序列化（PNG）」被实测否决，改为
> 二进制 Channel + 原始 RGBA（与桌面同一条 `STRF` 管线）。**

## 关键坑（用户要求记住）

- **PNG 压缩在手机上不可用于实时**：`png` crate 的**纯 Rust deflate** 编码 720p RGBA
  **每帧 ~3s**（`png` crate 用 miniz_oxide，无 SIMD/汇编加速）。实测手机端把显示压到
  **~0.3fps** 且画不出帧（`recvVideoW` 恒 0）。=> **结论：手机端不要用 PNG 逐帧压缩**；
  二进制 Channel（去掉 base64 33% 膨胀 + JSON 字符串 + `atob`）本身就解决了旧 11fps 的主因。
- **别把像素/编码活放在 MediaCodec 解码线程上**：早期把 `YUV→RGBA+PNG` 内联在 JNI
  （解码线程）里，720p 编码阻塞了解码 → 实测 `c2.mtk.avc.decoder inputFps=0`（停滞）。
  **必须 worker 线程 + 有界队列（`try_send`，落后丢帧），解码线程只拷贝+入队。**
- **`png::Encoder` 的 IEND 不能靠 `Writer` 的 `Drop` 写**：要显式 `writer.finish()`，否则
  PNG 缺 IEND，前端 `createImageBitmap` 解码失败、画不出帧。*（此为 PNG 路径的坑，随
  PNG 一并放弃。）*

## 最终架构（已落地）

- **Kotlin（`PlaybackPlugin.kt`）**：只留 MediaCodec 硬件解码薄壳，出 YUV → JNI 给 Rust。
- **Rust（`mobile_jni.rs`）**：`Java_..._nativeSubmitYuvFrame` 只做 `convert_byte_array` 拷贝 +
  `try_send` 入队（**解码线程零阻塞**）；background worker 线程循环：`yuv420_to_rgba_scaled`
  （`MAX_FRAME_W=1920`，只降不升→原生分辨率）→ `pack_frame`（magic "STRF"+w+h+pts+RGBA）
  → `frame_channel().send(bytes)` → `Channel<Vec<u8>>` 推前端。
- **前端（`subscribe.ts`/`ui.ts`）**：桌面与 Android **统一**走 `onVideoFrame`（STRF 解析）→
  `drawReceiveFrame`（`ImageData`/`putImageData`）。不再有 `IS_ANDROID` 的
  `createImageBitmap` 分支、不再有 `drawReceiveBitmap`。
- **显示通道**：前端 `newFrameChannel` 建的 `on_frame: Channel<Vec<u8>>`，经
  `start_receive_link` 传入，`spawn_android_playback` 调 `mobile_jni::set_frame_channel`
  注册，链路结束 `clear_frame_channel`。默认旧 `receive-frame`（base64 事件）已删。

## 实测数据（改造前 → 后）

| 指标 | 旧 base64 事件 | 二进制 Channel + 原始 RGBA 改造后 |
|------|----------------|--------------------------------|
| 手机显示帧率 | ~11 fps | 待测（预期明显提升） |
| 显示分辨率 | 480（全屏糊） | 720p 原生（上限 1920，源升 1080p 即 1080p） |
| 解码线程阻塞 | 无 | worker 卸载，不阻塞（decoder 回到 ~20+ fps） |

> 备注：`MAX_FRAME_W=1920` 只是上限，`yuv420_to_rgba_scaled` 内 `min(源宽, max)` 只降不升，
> 不会无谓放大。当前 PC 端源编码为 720p（Quality），**要真全屏 1080p 需 PC 源升 1080p**
> （会增加 PC 编码 CPU/带宽，是另一层权衡，留待 PC 侧提质）。
