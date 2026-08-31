package dev.stross.sender

import android.app.Activity
import android.content.pm.ActivityInfo
import android.content.Context
import android.media.AudioAttributes
import android.media.AudioFocusRequest
import android.media.AudioFormat
import android.media.AudioManager
import android.media.AudioTrack
import android.media.MediaCodec
import android.media.MediaFormat
import android.os.Build
import android.util.Base64
import android.util.Log
import android.view.WindowManager
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import android.view.Surface
import java.nio.ByteBuffer
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean

/**
 * Stross Android 播放插件 —— **MediaCodec/AudioTrack 系统 API 薄壳 + 硬件
 * Surface 直渲染**。
 *
 * 渲染路径（根治 WebView-canvas 像素路径的 CPU+内存双瓶颈）：
 * `MediaCodec` 解码器**配置为输出到 `SurfaceView` 的 Surface**
 * （`codec.configure(fmt, surface, null, 0)`），GPU 直出、零像素搬运（不再
 * YUV→RGBA→base64/二进制通道→canvas）。前端隐藏 canvas，原生 SurfaceView
 * 接管。
 *
 * - `feedVideo`（Rust→Kotlin）：把 H.264 帧**入队立即返回**（不再在命令线程
 *   同步解码）；csd（SPS/PPS）与尺寸由 Rust 解析随帧下发，这里直接用来建
 *   解码器。
 * - 独立解码线程消费队列：`dequeueInputBuffer` 用**短超时**（解码器忙即丢帧，
 *   不阻塞）；输出 buffer 直接 `releaseOutputBuffer(idx, true)` 渲染到 Surface，
 *   每帧回调 `nativeDecodedFrame()`（JNI 直调 Rust）写解码统计。
 * - `feedAudio`：ADTS→AAC→PCM→AudioTrack（队列线程写设备，已有）。
 * - 本文件不再包含：BitReader/SPS 位级解析（Rust `nal.rs` 已有）、
 *   逐像素 YUV→RGBA 转换（Rust `yuv.rs`）、base64/二进制帧回传。
 *
 * JNI（由 Rust `mobile_jni.rs` 导出，仅解码统计回写）：
 * ```kotlin
 * private external fun nativeDecodedFrame()
 * ```
 */
@TauriPlugin
class PlaybackPlugin(activity: Activity) : Plugin(activity) {
    private val host: Activity = activity
    companion object {
        private const val TAG = "StrossPlay"
        /** 视频输入队列上界：解码跟不上时丢帧，避免积压无限膨胀（内存有界）。 */
        private const val VIDEO_QUEUE_CAP = 8
        /** `dequeueInputBuffer` 超时：解码器忙时到此即丢帧（不再阻塞 5s）。 */
        private const val INPUT_TIMEOUT_US = 2_000L
    }

    @InvokeArg
    class StartArgs {
        var audio: Boolean = false
    }

    @InvokeArg
    class FeedArgs {
        var d: String = "" // base64
        var k: Boolean = false
        var c: Boolean = false
        var p: Long = 0
        // Rust 解析 SPS 后下发：csd（SPS+PPS base64）与编码尺寸（宽高）
        var w: Int = 0
        var h: Int = 0
        var csd: String? = null
    }

    /** 一帧视频输入（解码线程消费）。 */
    private class VideoJob(
        val data: ByteArray,
        val keyframe: Boolean,
        val isConfig: Boolean,
        val ptsMs: Long,
        val w: Int,
        val h: Int,
        val csd: ByteArray?,
    )

    private val running = AtomicBoolean(false)

    // 视频解码：有界队列 + 独立解码线程（命令线程只入队立即返回）
    private val videoQueue = LinkedBlockingQueue<VideoJob>(VIDEO_QUEUE_CAP)
    private var videoThread: Thread? = null

    // 视频解码器
    private var vDecoder: MediaCodec? = null
    private var vWidth = 0
    private var vHeight = 0
    private val vLock = Any()

    // 音频与焦点
    private var aDecoder: MediaCodec? = null
    private var audioTrack: AudioTrack? = null
    private val audioQueue = LinkedBlockingQueue<ByteArray>()
    private var audioThread: Thread? = null
    private var audioSampleRate = 48_000
    private var audioChannels = 2
    private var audioManager: AudioManager? = null
    private var audioFocusRequest: AudioFocusRequest? = null
    // ------------------------------------------------------------------
    // JNI（Rust mobile_jni.rs）：仅解码统计回写（Surface 直渲染无像素回传）
    // ------------------------------------------------------------------

    private external fun nativeDecodedFrame()

    // ------------------------------------------------------------------
    // 生命周期
    // ------------------------------------------------------------------

    @Command
    fun startPlayback(invoke: Invoke) {
        val args = invoke.parseArgs(StartArgs::class.java)
        running.set(true)
        setKeepScreenOn(true)
        // 显示原生 SurfaceView（触发 surface 创建，MediaCodec 随后输出到它）
        showSurfaceView()
        if (args.audio) {
            requestAudioFocus()
            startAudioTrack()
        }
        startVideoThread()
        invoke.resolve(JSObject().apply { put("started", true) })
    }

    /** 显示 `MainActivity` 的 SurfaceView（不定位——前端随后上报播放区矩形）。 */
    private fun showSurfaceView() {
        (host as? MainActivity)?.showPlaybackSurface()
    }

    // ------------------------------------------------------------------
    // 原生 SurfaceView 定位 / 全屏（前端经 Rust 命令下发）
    // ------------------------------------------------------------------

    @InvokeArg
    class SurfaceBoundsArgs {
        var x: Int = 0
        var y: Int = 0
        var w: Int = 0
        var h: Int = 0
    }

    @InvokeArg
    class FullscreenArgs {
        var active: Boolean = false
    }

    @Command
    fun setSurfaceBounds(invoke: Invoke) {
        val args = invoke.parseArgs(SurfaceBoundsArgs::class.java)
        (host as? MainActivity)?.let { act ->
            if (!act.isPlaybackSurfaceShown()) act.showPlaybackSurface()
            act.showPlaybackSurface(android.graphics.Rect(args.x, args.y, args.x + args.w, args.y + args.h))
        }
        invoke.resolve(JSObject().apply { put("ok", true) })
    }

    @Command
    fun setNativeFullscreen(invoke: Invoke) {
        val args = invoke.parseArgs(FullscreenArgs::class.java)
        (host as? MainActivity)?.let { act ->
            if (args.active) act.enterPlaybackFullscreen() else act.exitPlaybackFullscreen()
        }
        invoke.resolve(JSObject().apply { put("ok", true) })
    }

    @Command
    fun hideSurface(invoke: Invoke) {
        (host as? MainActivity)?.hidePlaybackSurface()
        invoke.resolve(JSObject().apply { put("ok", true) })
    }

    @InvokeArg
    class OrientationArgs {
        var orientation: String = "unspecified"
    }

    @Command
    fun setOrientation(invoke: Invoke) {
        val args = invoke.parseArgs(OrientationArgs::class.java)
        try {
            host.runOnUiThread {
                when (args.orientation) {
                    "landscape" -> host.requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_SENSOR_LANDSCAPE
                    "portrait" -> host.requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_SENSOR_PORTRAIT
                    else -> host.requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_UNSPECIFIED
                }
            }
        } catch (e: Exception) {
            Log.w(TAG, "设置屏幕方向异常: ${e.message}")
        }
        invoke.resolve(JSObject().apply { put("success", true) })
    }

    @Command
    fun stopPlayback(invoke: Invoke) {
        if (running.compareAndSet(true, false)) {
            stopEverything()
        }
        invoke.resolve(JSObject().apply { put("stopped", true) })
    }

    // ------------------------------------------------------------------
    // 视频：入队立即返回（命令线程不被解码拖住）→ 解码线程 → JNI 直调 Rust
    // ------------------------------------------------------------------

    @Command
    fun feedVideo(invoke: Invoke) {
        val args = invoke.parseArgs(FeedArgs::class.java)
        if (!running.get()) {
            invoke.resolve(JSObject())
            return
        }
        try {
            val bytes = Base64.decode(args.d, Base64.NO_WRAP)
            val csd = args.csd?.let { Base64.decode(it, Base64.NO_WRAP) }
            if (args.csd != null) Log.i(TAG, "feedVideo 收到关键帧 csd w=${args.w} h=${args.h} len=${bytes.size}")
            val job = VideoJob(bytes, args.k, args.c, args.p, args.w, args.h, csd)
            // 有界队列：满时**有选择地丢帧**（优先丢非关键帧，保关键帧对齐）。
            // 队列满且新帧关键帧时清队重入（重建参考），否则丢弃该帧。
            if (!videoQueue.offer(job)) {
                if (job.keyframe || job.isConfig) {
                    videoQueue.clear()
                    videoQueue.offer(job)
                } else {
                    Log.d(TAG, "视频队列满，丢非关键帧（pts=${job.ptsMs}）")
                }
            }
        } catch (e: Exception) {
            Log.w(TAG, "feedVideo 入队失败: ${e.message}")
        }
        invoke.resolve(JSObject())
    }

    /** 视频解码线程：消费队列 → MediaCodec → YUV → JNI → Rust。 */
    private fun startVideoThread() {
        if (videoThread != null && videoThread?.isAlive == true) {
            return
        }
        videoThread = Thread {
            try {
                Log.i(TAG, "视频解码线程启动")
                while (running.get()) {
                    val job = videoQueue.poll(200, TimeUnit.MILLISECONDS) ?: continue
                    handleVideoJob(job)
                }
            } catch (e: Exception) {
                Log.w(TAG, "视频解码线程退出: ${e.message}")
            } finally {
                Log.i(TAG, "视频解码线程结束")
                releaseVideoDecoder()
            }
        }.apply { start() }
    }

    private fun handleVideoJob(job: VideoJob) {
        val codec: MediaCodec
        synchronized(vLock) {
            if (vDecoder == null) {
                val csd = job.csd ?: run { if (job.keyframe) Log.w(TAG, "关键帧无 csd，等带 csd 的关键帧"); return }
                try {
                    val ok = createVideoDecoder(csd, job.w, job.h)
                    if (!ok) return // Surface 未就绪：丢弃该帧，等下一关键帧重试
                } catch (e: Exception) {
                    Log.w(TAG, "建解码器失败: ${e.message}")
                    return
                }
            }
            codec = vDecoder ?: return
        }
        if (job.isConfig) {
            // 配置帧只用于建解码器（上面已完成），不喂数据
            return
        }
        val inIdx = codec.dequeueInputBuffer(INPUT_TIMEOUT_US)
        if (inIdx < 0) {
            // 解码器忙：丢帧（Rust 侧积压跳帧已在源头减负，此处兜底）
            Log.d(TAG, "解码器输入忙，丢帧 (pts=${job.ptsMs})")
            return
        }
        val inBuf = codec.getInputBuffer(inIdx) ?: return
        inBuf.clear()
        inBuf.put(job.data)
        codec.queueInputBuffer(inIdx, 0, job.data.size, job.ptsMs * 1000, 0)
        drainVideoOutput(codec)
    }

    /** Surface 渲染到 `MainActivity` 的 SurfaceView；输出即硬件画面，无像素回传。 */
    private fun drainVideoOutput(codec: MediaCodec) {
        val info = MediaCodec.BufferInfo()
        while (true) {
            val idx = codec.dequeueOutputBuffer(info, 0)
            when {
                idx == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED -> {
                    val fmt = codec.outputFormat
                    vWidth = fmt.getInteger(MediaFormat.KEY_WIDTH)
                    vHeight = fmt.getInteger(MediaFormat.KEY_HEIGHT)
                    Log.i(TAG, "解码器输出格式: ${vWidth}x$vHeight")
                }
                idx >= 0 -> {
                    val flags = info.flags
                    if (flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG == 0) {
                        // 渲染到 Surface（releaseOutputBuffer(idx, true)），逐帧回写解码统计
                        codec.releaseOutputBuffer(idx, true)
                        try { nativeDecodedFrame() } catch (e: Throwable) { Log.w(TAG, "JNI 解码统计失败: ${e.message}") }
                    } else {
                        codec.releaseOutputBuffer(idx, false)
                    }
                }
                else -> return // 无更多输出
            }
        }
    }

    /** 用 Rust 解析好的 csd（SPS+PPS）与宽高创建 AVC 解码器——**输出到 Surface**。
     *  返回 false 表示 Surface 尚未就绪（解码线程随后丢弃该帧，等下一关键帧重试）。 */
    private fun createVideoDecoder(csd: ByteArray, w: Int, h: Int): Boolean {
        val surface = acquireSurface() ?: return false
        releaseVideoDecoder()
        val width = if (w > 0) w else 1920
        val height = if (h > 0) h else 1080
        val fmt = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, width, height).apply {
            setByteBuffer("csd-0", ByteBuffer.wrap(csd))
            setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, 2 * 1024 * 1024)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                try { setInteger("low-latency", 1) } catch (_: Exception) {}
            }
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
                try { setInteger("priority", 0) } catch (_: Exception) {}
            }
        }
        val codec = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
        // 直接输出到 Surface（硬件直渲染），渲染目标由 SurfaceView 管理
        Log.i(TAG, "createVideoDecoder configure surface=$surface ${width}x$height")
        codec.configure(fmt, surface, null, 0)
        codec.start()
        vDecoder = codec
        vWidth = 0
        vHeight = 0
        Log.i(TAG, "视频解码器就绪（Surface 直渲染，${width}x$height）")
        return true
    }

    /** 阻塞等待 `MainActivity` 的 Surface 就绪（有限重试，成功返回非 null）。 */
    private fun acquireSurface(): Surface? {
        val act = host as? MainActivity ?: run { Log.w(TAG, "acquireSurface: host 不是 MainActivity"); return null }
        for (i in 0 until 40) {
            val holder = act.playbackSurfaceView?.holder ?: continue
            val s = try { holder.surface } catch (e: Exception) { Log.w(TAG, "holder.surface 异常: ${e.message}"); null }
            if (s != null && s.isValid) return s
            try { Thread.sleep(25) } catch (_: InterruptedException) { return null }
        }
        Log.w(TAG, "acquireSurface 超时（1s）")
        return null
    }

    // ------------------------------------------------------------------
    // 音频：ADTS → raw AAC → MediaCodec → PCM → AudioTrack（队列线程）
    // ------------------------------------------------------------------

    private fun startAudioTrack() {
        if (audioTrack != null && audioThread != null && audioThread?.isAlive == true) {
            return
        }
        val minBuf = AudioTrack.getMinBufferSize(
            audioSampleRate,
            AudioFormat.CHANNEL_OUT_STEREO,
            AudioFormat.ENCODING_PCM_16BIT
        )
        if (minBuf <= 0) {
            Log.w(TAG, "AudioTrack 不支持 48k 立体声")
            return
        }
        val track = AudioTrack.Builder()
            .setAudioAttributes(
                AudioAttributes.Builder()
                    .setUsage(AudioAttributes.USAGE_MEDIA)
                    .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
                    .setFlags(AudioAttributes.FLAG_LOW_LATENCY)
                    .build()
            )
            .setAudioFormat(
                AudioFormat.Builder()
                    .setSampleRate(audioSampleRate)
                    .setChannelMask(AudioFormat.CHANNEL_OUT_STEREO)
                    .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                    .build()
            )
            .setBufferSizeInBytes(minBuf * 2)
            .apply {
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                    setPerformanceMode(AudioTrack.PERFORMANCE_MODE_LOW_LATENCY)
                }
            }
            .build()
        audioTrack = track
        track.play()
        audioThread = Thread { audioWriteLoop() }.apply { start() }
        Log.i(TAG, "AudioTrack 就绪（低延迟模式）")
    }

    private fun audioWriteLoop() {
        try {
            while (running.get()) {
                val pcm = audioQueue.poll(200, TimeUnit.MILLISECONDS) ?: continue
                val track = audioTrack ?: break
                var off = 0
                while (off < pcm.size && running.get()) {
                    val n = track.write(pcm, off, pcm.size - off)
                    if (n <= 0) break
                    off += n
                }
            }
        } catch (e: Exception) {
            Log.w(TAG, "音频写线程退出: ${e.message}")
        }
    }

    @Command
    fun feedAudio(invoke: Invoke) {
        val args = invoke.parseArgs(FeedArgs::class.java)
        if (!running.get()) {
            invoke.resolve(JSObject())
            return
        }
        try {
            val bytes = Base64.decode(args.d, Base64.NO_WRAP)
            decodeAudioFrame(bytes, args.p)
        } catch (e: Exception) {
            Log.w(TAG, "feedAudio 失败: ${e.message}")
        }
        invoke.resolve(JSObject())
    }

    /** ADTS → raw AAC → 解码 → PCM 入队（AudioTrack 由队列线程写）。 */
    private fun decodeAudioFrame(adts: ByteArray, ptsMs: Long) {
        if (adts.size < 7 || (adts[0].toInt() and 0xFF) != 0xFF) return // 非法 ADTS
        val profile = ((adts[2].toInt() and 0xC0) shr 6) + 1 // ADTS profile → AAC object type
        val freqIdx = (adts[2].toInt() and 0x3C) shr 2
        val channels = ((adts[2].toInt() and 0x01) shl 2) or ((adts[3].toInt() and 0xC0) shr 6)
        if (channels == 0) return
        audioSampleRate = sampleRateOf(freqIdx) ?: audioSampleRate

        val codec = aDecoder ?: createAudioDecoder(profile, freqIdx, channels) ?: return
        val inIdx = codec.dequeueInputBuffer(INPUT_TIMEOUT_US)
        if (inIdx < 0) return
        val inBuf = codec.getInputBuffer(inIdx) ?: return
        inBuf.clear()
        inBuf.put(adts, 7, adts.size - 7) // 剥 ADTS 头，喂 raw AAC
        codec.queueInputBuffer(inIdx, 0, adts.size - 7, ptsMs * 1000, 0)

        val info = MediaCodec.BufferInfo()
        while (true) {
            val idx = codec.dequeueOutputBuffer(info, 0)
            if (idx >= 0) {
                if (info.size > 0) {
                    val pcm = ByteArray(info.size)
                    codec.getOutputBuffer(idx)?.apply {
                        position(info.offset)
                        get(pcm)
                    }
                    audioQueue.offer(pcm)
                }
                codec.releaseOutputBuffer(idx, false)
            } else {
                break
            }
        }
    }

    private fun createAudioDecoder(profile: Int, freqIdx: Int, channels: Int): MediaCodec? {
        val sampleRate = sampleRateOf(freqIdx) ?: return null
        val asc = ByteArray(2)
        asc[0] = (((profile shl 3) or (freqIdx shr 1)) and 0xFF).toByte()
        asc[1] = ((((freqIdx and 0x01) shl 7) or (channels shl 3)) and 0xFF).toByte()
        val fmt = MediaFormat.createAudioFormat(MediaFormat.MIMETYPE_AUDIO_AAC, sampleRate, channels).apply {
            setByteBuffer("csd-0", ByteBuffer.wrap(asc))
            setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, 16 * 1024)
        }
        val codec = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_AUDIO_AAC)
        codec.configure(fmt, null, null, 0)
        codec.start()
        aDecoder = codec
        audioSampleRate = sampleRate
        audioChannels = channels
        Log.i(TAG, "音频解码器就绪: ${sampleRate}Hz ${channels}ch profile=$profile")
        return codec
    }

    private fun sampleRateOf(freqIdx: Int): Int? = when (freqIdx) {
        0 -> 96000; 1 -> 88200; 2 -> 64000; 3 -> 48000; 4 -> 44100
        5 -> 32000; 6 -> 24000; 7 -> 22050; 8 -> 16000; 9 -> 12000
        10 -> 11025; 11 -> 8000; 12 -> 7350; else -> null
    }
    private fun stopEverything() {
        setKeepScreenOn(false)
        releaseAudioFocus()
        (host as? MainActivity)?.hidePlaybackSurface()
        synchronized(vLock) {
            releaseVideoDecoder()
        }
        videoQueue.clear()
        audioQueue.clear()
        audioThread?.interrupt()
        audioThread = null
        try { aDecoder?.stop() } catch (_: Exception) {}
        try { aDecoder?.release() } catch (_: Exception) {}
        aDecoder = null
        try { audioTrack?.stop() } catch (_: Exception) {}
        try { audioTrack?.release() } catch (_: Exception) {}
        audioTrack = null
    }

    private fun setKeepScreenOn(enable: Boolean) {
        try {
            host.runOnUiThread {
                if (enable) {
                    host.window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
                } else {
                    host.window.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
                }
            }
        } catch (e: Exception) {
            Log.w(TAG, "屏幕常亮设置异常: ${e.message}")
        }
    }

    private fun requestAudioFocus() {
        try {
            if (audioManager == null) {
                audioManager = host.getSystemService(Context.AUDIO_SERVICE) as? AudioManager
            }
            val am = audioManager ?: return
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                val req = AudioFocusRequest.Builder(AudioManager.AUDIOFOCUS_GAIN_TRANSIENT_MAY_DUCK)
                    .setAudioAttributes(
                        AudioAttributes.Builder()
                            .setUsage(AudioAttributes.USAGE_MEDIA)
                            .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
                            .build()
                    )
                    .setOnAudioFocusChangeListener { focusChange ->
                        when (focusChange) {
                            AudioManager.AUDIOFOCUS_LOSS_TRANSIENT_CAN_DUCK -> {
                                try { audioTrack?.setVolume(0.25f) } catch (_: Exception) {}
                            }
                            AudioManager.AUDIOFOCUS_GAIN -> {
                                try { audioTrack?.setVolume(1.0f) } catch (_: Exception) {}
                            }
                            AudioManager.AUDIOFOCUS_LOSS,
                            AudioManager.AUDIOFOCUS_LOSS_TRANSIENT -> {
                                try { audioTrack?.setVolume(0.0f) } catch (_: Exception) {}
                            }
                        }
                    }
                    .build()
                audioFocusRequest = req
                am.requestAudioFocus(req)
            }
        } catch (e: Exception) {
            Log.w(TAG, "音频焦点请求异常: ${e.message}")
        }
    }

    private fun releaseAudioFocus() {
        try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                audioFocusRequest?.let { audioManager?.abandonAudioFocusRequest(it) }
                audioFocusRequest = null
            }
        } catch (e: Exception) {
            Log.w(TAG, "音频焦点释放异常: ${e.message}")
        }
    }

    private fun releaseVideoDecoder() {
        try {
            vDecoder?.stop()
        } catch (_: Exception) {
        }
        try {
            vDecoder?.release()
        } catch (_: Exception) {
        }
        vDecoder = null
        vWidth = 0
        vHeight = 0
    }
}