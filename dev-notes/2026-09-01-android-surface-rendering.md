# Surface 渲染落地（WebView-canvas 像素路径的终结）

> 主题：Android 手机端「订阅→播放」从 canvas 像素路径切到 MediaCodec→SurfaceView
> 硬件直渲染。替代 `2026-08-31-android-phone-display-binary-channel.md`（c）的第二阶段。

## 结论：为什么最终走 Surface

- **canvas 像素路径（解码→YUV→RGBA→base64/二进制通道→canvas）在手机上做全分辨率
  走不通**：
  - base64-480：11fps，但（base64 膨胀 + atob + putImageData）慢；
  - 二进制 Channel + 原始 RGBA（720p）：decode 正常（21fps），但**每帧 3.7MB**
    STRF-RGBA 经 Channel→canvas，内存爆（80MB+ native）→ OOM/崩溃/黑屏；
  - PNG 逐帧压缩：`png` crate 纯 Rust deflate 在手机上编码 720p 每帧 ~3s，不可用。
- **正解 = 硬件 Surface 渲染**：`MediaCodec.configure(fmt, surface, null, 0)` 让解码器
  直接画到 `SurfaceView` 的 Surface，GPU 直出、零像素搬运。顺带解决「全屏 bug」
  （全屏由原生视图控制，不走 WebView CSS）。

## 实现要点

1. **`MainActivity.kt`（真源，`android/*.kt`）**：`super.onCreate` 后往窗口
   `decorView` 程序化加 `SurfaceView`：
   - `setZOrderOnTop(true)` → 硬件 overlay，保证盖在 WebView 之上；
   - **初始 1×1 占位（VISIBLE，不是 GONE）**：GONE 会销毁 surface，导致 MediaCodec
     配置时无有效 surface。1×1 保持 surface 常有效且在小角不可见。
   - 暴露 `showPlaybackSurface(rect)` / `showPlaybackSurface()`（reset 1×1）/
     `hidePlaybackSurface()` / `enterPlaybackFullscreen()` / `exitPlaybackFullscreen()`。
   - ⚠️ `window.decorView` 是 `View` 不是 `ViewGroup`，`addView` 要先 `as ViewGroup`。
2. **`PlaybackPlugin.kt`**：
   - `codec.configure(fmt, surface, null, 0)` 输出到 Surface；输出 buffer 用
     `releaseOutputBuffer(idx, **true**)` 渲染。
   - `acquireSurface()`：解码线程阻塞有限重试等 `holder.surface` 有效。
   - 删掉 YUV→JNI 回传（`nativeSubmitYuvFrame`）与读 YUV 段；改为每渲染一帧调
     `nativeDecodedFrame()`（JNI 直调 Rust 写解码统计）。
   - 新增命令 `setSurfaceBounds` / `setNativeFullscreen` / `hideSurface`。
3. **`mobile_jni.rs`**：删像素桥（YUV→RGBA→pack_frame→Channel），只留
   `nativeDecodedFrame()` JNI + `set_active_link/clear_active_link`（多端点链接路由）。
4. **`mobile.rs` / `receive.rs`**：`spawn_android_playback` 增 `link_id` 参数，登记活动链路；
   `on_frame` Channel 参数保留但 Android 忽略。
5. **内核**：`Kernel::note_android_decoded_frame_on(link_id)`（空 = `main` 槽），
   多端点链接把解码统计写到正确链路。
6. **前端 `subscribe.ts` / `ui.ts`**：
   - Android 隐藏 canvas 绘制；据 `receive_links` 的 `decodedVideo` 统计判定「有画面」，
     并**自动把 `activeVideoLink` 设为有解码帧的链路**（原本靠 `onVideoFrame` 回调，
     Surface 路径无帧回调 → 主动在 poll 里赋值）。
   - `syncAndroidSurface()`：有画面时取 `#recv-canvas-wrap` 的 rect，换算物理 px
     （×devicePixelRatio）发 `set_playback_surface_bounds`；无画面/停止时 `hide_playback_surface`。
   - 全屏：Android 走 `set_native_fullscreen`（Surface 铺满 + 隐藏系统栏），
     CSS `.canvas-wrap.fs` 仍在但被 Surface 盖住；退出全屏恢复系统栏 + 重新发播放区矩形。
   - 监听 `resize`/`orientationchange` 重定位 Surface。

## 关键坑（再踩必看）

- **`decorView` 不能直接 `addView`**（类型 `View`），要 `as ViewGroup`。
- **SurfaceView 用 GONE 起 + `SurfaceHolder.Callback`**：GONE 时无 surface（不可配置），
  播放一开始**给真实尺寸**（铺满窗口）才触发 `surfaceCreated`；解码器等 surface
  就绪再配置。（曾用 1×1 VISIBLE 占位——本机不触发 surface 创建，务必别走那条。）
- **`releaseOutputBuffer(idx, true)` 渲染**（false 是丢弃/不渲染）——Surface 输出
  下必须 `true` 才出画面。
- **Surface 在 WebView 之外、release 无 CDP**：release 只能人眼验证；debug 有 CDP
  但 Surface 画面不反映在 DOM，需 `adb exec-out screencap` 看屏。
- **`activeVideoLink` 靠 `onVideoFrame` 赋值在 Surface 路径失效** → 要在 poll 里
  decode 统计>0 时赋值。
- **原生 Surface 置顶后，Surface 区域内的 WebView 元素不可点击**（触摸被 Surface
  消费）。播放区（canvas-wrap）内原本 hover 才显示的 `recv-controls` 在手机上本就
  无 hover，不构成回归；`stage-head` 头部「停止全部」在播放区外，仍可点。

## 验证清单（真机）

① PC→手机播放流畅、非黑屏；② 全屏（原生）正确、系统栏隐藏、上下黑边/等比；
③ 退出播放 Surface 隐藏、回到 WebView UI；④ 反向（手机→PC）仍正常；⑤ 音频同步。

## 真机实测修的两个根因（2026-09-01 会话）

**症状**：手机订阅后无画面（解码器从未建、`surfaceCreated` 从不回调）。

1. **1×1 `SurfaceView` 不触发 surface 创建**（本机实测 `surfaceCreated` 从不回调，
   `holder.surface` invalid）。根因：SurfaceView 置 `GONE` 或极小尺寸（1×1）时
   SurfaceFlinger 不为其建 surface。**修法**：播放开始时给真实尺寸（铺满窗口），
   surface 创建后再由前端按播放区矩形重定位；用 `SurfaceHolder.Callback` 跟踪
   `surfaceCreated/Destroyed`，解码器在 surface 就绪后才配置。
2. **前端在缓冲期隐藏 surface 形成死锁**：`syncAndroidSurface` 原来把「显示 surface」
   门在 `decodedVideo > 0`（首发画面），而解码需要 surface 先存在——于是表面一直
   不显示、解码永不启动。**修法**：按「视频链路」而非「已出画面」决定显示——订阅
   时记录端点 `kind`（screen/camera→`link.video`），视频链路在缓冲期就显示 surface
   （发播放区矩形），纯音频链路才隐藏。

**诊断手法**：`adb logcat -s StrossSurface`（surface 生命周期）+ `-s StrossPlay`
（关键帧/解码器就绪/输出格式）。`surfaceChanged 1080x2378→990x557` 即前端定位生效。

## 全屏退出崩溃（2026-09-01 会话）

**症状**：原生全屏可进，但没有可点的退出控件（Surface 置顶盖住 WebView 控制条）；
按系统返回键退出时**崩溃**（`FORTIFY: pthread_mutex_lock called on a destroyed mutex`
+ `BufferQueue has been abandoned`）。

**根因**：返回键默认行为 = `finish` activity → SurfaceView 的 surface 被销毁，而
MediaCodec 解码器仍在（Rust 接收会话继续喂帧）向其渲染 → 向已废弃 buffer queue
渲染 → 原生 mutex 竞态崩溃。

**修法**：
1. **`MainActivity.onBackPressed` 全屏时优先退出全屏**（恢复系统栏 + 定向回
   unspecified + 恢复播放区矩形），保持 activity 存活、surface 有效——既不崩溃，
   也给了「退出全屏」的功能（返回键）。
2. **退出全屏通知前端**：Kotlin `notifyFullscreenExited` → JNI
   `nativeFullscreenExited`（mobile_jni.rs）→ 发 Tauri 事件
   `native-fullscreen-changed {active:false}` → 前端 `handleNativeFullscreenChanged`
   复位 `fsActive`、撤 `.fs`、重定位 surface。
3. `MainActivity` 记录 `lastPlayerRect`（showPlaybackSurface(rect) 时存），退出全屏
   后恢复定位。

**要点**：Android 原生全屏下，Surface 硬件 overlay 永远盖在 WebView 之上——WebView
内控制条不可点。**退出全屏只能靠系统返回键/手势**（或原生再画退出按钮）。返回键
必须被拦截为「退出全屏」，不能落到默认 finish。
