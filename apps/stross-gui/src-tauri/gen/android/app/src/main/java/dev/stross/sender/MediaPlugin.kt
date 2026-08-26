package dev.stross.sender

import android.Manifest
import android.app.Activity
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.AudioFormat
import android.media.AudioRecord
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.media.MediaRecorder
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.util.Base64
import android.util.Log
import androidx.core.content.ContextCompat
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Channel
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import app.tauri.plugin.PluginManager
import java.util.concurrent.atomic.AtomicBoolean

/**
 * Stross Android 采集插件（由 Rust 侧 `register_android_plugin` 自动实例化并注册，
 * 无需在 MainActivity 手动 addPlugin；构造签名必须为 `(Activity)`）。
 *
 * - 屏幕：MediaProjection 授权（PluginManager.startActivityForResult）→ 前台服务
 *   （API 34+ 强制）→ VirtualDisplay 直连 MediaCodec(H.264) 编码器输入面
 * - 麦克风：AudioRecord（PluginManager.requestPermissions）→ MediaCodec(AAC) → 加 ADTS 头
 *
 * 编码帧通过 Tauri [Channel] 回传 Rust（base64 JSON，见 `mobile.rs`）。
 */
@TauriPlugin
class MediaPlugin(activity: Activity) : Plugin(activity) {

    companion object {
        private const val TAG = "StrossMedia"
    }

    @InvokeArg
    class CaptureArgs {
        var streamId: String = ""
        var width: Int = 1280
        var height: Int = 720
        var fps: Int = 30
        var bitrateKbps: Int = 2500
        var withAudio: Boolean = true
        var channel: Channel? = null
        /** 纯麦克风采集（B2 手机反向推流）：跳过屏幕录制授权/前台服务。 */
        var micOnly: Boolean = false
    }

    // Plugin 基类的 activity 是 private，这里保存一份供本类使用
    private val host: Activity = activity

    private var channel: Channel? = null
    private val running = AtomicBoolean(false)

    // 编码参数
    private var width = 1280
    private var height = 720
    private var fps = 30
    private var bitrateKbps = 2500
    private var withAudio = true
    /** 当前是否为纯麦克风采集（B2：无屏幕授权/前台服务/虚拟显示）。 */
    private var micOnly = false

    // 视频
    private var encoder: MediaCodec? = null
    private var virtualDisplay: VirtualDisplay? = null
    private var projection: MediaProjection? = null
    private var encodeThread: HandlerThread? = null

    // 音频
    private var audioRecord: AudioRecord? = null
    private var audioEncoder: MediaCodec? = null
    private var audioThread: Thread? = null
    private var aacProfile = 1
    private var aacFreqIdx = 3 // 48000
    private var aacChannels = 2

    private var pendingInvoke: Invoke? = null

    // ------------------------------------------------------------------
    // 命令
    // ------------------------------------------------------------------

    @Command
    fun startCapture(invoke: Invoke) {
        Log.i(TAG, "startCapture 进入 thread=${Thread.currentThread().name}")
        try {
            val args = invoke.parseArgs(CaptureArgs::class.java)
            Log.i(TAG, "startCapture args: chan=${args.channel != null} w=${args.width} h=${args.height} audio=${args.withAudio} running=$running")
            val chan = args.channel
            if (chan == null) {
                invoke.reject("缺少 channel 参数")
                return
            }
            if (running.get()) {
                invoke.reject("已经在采集")
                return
            }
            channel = chan
            width = args.width.coerceIn(320, 3840)
            height = args.height.coerceIn(240, 2160)
            fps = args.fps.coerceIn(10, 60)
            bitrateKbps = args.bitrateKbps.coerceIn(200, 20_000)
            withAudio = args.withAudio || args.micOnly

            // 纯麦克风采集（B2）：不请求屏幕录制授权/前台服务/虚拟显示，
            // 直接申请麦克风权限并启动 AudioRecord 编码。立即 resolve
            // （Rust 侧无需等待系统弹窗；真实状态由 t=9 控制帧回报）。
            if (args.micOnly) {
                if (!running.compareAndSet(false, true)) {
                    invoke.reject("已经在采集")
                    return
                }
                micOnly = true
                invoke.resolve(JSObject().apply { put("started", true) })
                requestMicAndStart()
                return
            }
            micOnly = false

            // 屏幕录制授权（系统弹窗）。注意：startActivityForResult 必须在主线程
            // （Rust 侧 run_mobile_plugin 在 blocking 线程调用本命令），否则不弹窗；
            // OPPO 等 ROM 会把前台服务启动延迟数秒（startForegroundDelayMs），
            // 因此立即 resolve 并在后台线程等待投影，避免阻塞主线程与 Rust 侧。
            pendingInvoke = invoke
            Log.i(TAG, "startCapture 请求屏幕录制授权")
            val pm = host.getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
            host.runOnUiThread {
                try {
                    PluginManager.startActivityForResult(pm.createScreenCaptureIntent()) { result ->
                        val pend = pendingInvoke
                        pendingInvoke = null
                        if (result.resultCode == Activity.RESULT_OK && result.data != null) {
                            Log.i(TAG, "屏幕录制授权通过")
                            pend?.resolve(JSObject().apply { put("started", true) })
                            val code = result.resultCode
                            val data = result.data!!
                            Thread {
                                startProjectionService(code, data)
                            }.start()
                        } else {
                            Log.w(TAG, "屏幕录制授权被拒绝")
                            pend?.reject("用户拒绝了屏幕录制授权")
                            // 同步回报前端：采集未启动及原因（前端靠 t=9 控制帧更新状态）
                            sendControl(JSObject().apply {
                                put("t", 9)
                                put("started", false)
                                put("err", "用户拒绝了屏幕录制授权")
                            })
                        }
                    }
                } catch (e: Throwable) {
                    Log.e(TAG, "startActivityForResult 异常: ${e.message}", e)
                    pendingInvoke?.reject("授权请求异常: ${e.message}")
                    pendingInvoke = null
                }
            }
        } catch (e: Throwable) {
            Log.e(TAG, "startCapture 异常: ${e.message}", e)
            try {
                invoke.reject("startCapture 异常: ${e.message}")
            } catch (_: Exception) {
            }
        }
    }

    @Command
    fun stopCapture(invoke: Invoke) {
        stopEverything()
        invoke.resolve(JSObject().apply { put("stopped", true) })
    }

    // ------------------------------------------------------------------
    // 屏幕采集
    // ------------------------------------------------------------------

    /** API 34+ 要求先启动 mediaProjection 类型的前台服务再 getMediaProjection。 */
    private fun startProjectionService(resultCode: Int, data: Intent) {
        // 先重置等待状态（服务端 static 状态可能残留上次推流）
        ProjectionService.resetProjection()
        val intent = Intent(host, ProjectionService::class.java).apply {
            putExtra(ProjectionService.EXTRA_RESULT_CODE, resultCode)
            putExtra(ProjectionService.EXTRA_RESULT_DATA, data)
        }
        ContextCompat.startForegroundService(host, intent)
        // OPPO 等 ROM 前台服务启动有延迟（可达 10 秒+），耐心等待
        val projection = ProjectionService.awaitProjection(40_000)
        if (projection == null) {
            Log.e(TAG, "获取 MediaProjection 超时（40s）")
            // 通知 Rust/前端：采集实际未启动
            sendControl(JSObject().apply {
                put("t", 9)
                put("started", false)
                put("err", "屏幕投影获取超时，请重试")
            })
            channel = null
            return
        }
        startProjection(projection)
    }

    private fun startProjection(proj: MediaProjection) {
        if (!running.compareAndSet(false, true)) {
            return
        }
        try {
            startProjectionInner(proj)
        } catch (e: Exception) {
            // 任何启动失败都不能让线程/进程崩溃：回传状态帧，前端展示原因
            Log.e(TAG, "启动采集失败: ${e.message}")
            sendControl(JSObject().apply {
                put("t", 9)
                put("started", false)
                put("err", "启动采集失败: ${e.message}")
            })
            running.set(false)
            try {
                encoder?.release()
            } catch (_: Exception) {
            }
            encoder = null
            try {
                virtualDisplay?.release()
            } catch (_: Exception) {
            }
            virtualDisplay = null
            try {
                proj.stop()
            } catch (_: Exception) {
            }
            channel = null
        }
    }

    private fun startProjectionInner(proj: MediaProjection) {
        projection = proj
        encodeThread = HandlerThread("stross-video-encoder").apply { start() }
        val handler = Handler(encodeThread!!.looper)

        val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, width, height).apply {
            setInteger(
                MediaFormat.KEY_COLOR_FORMAT,
                MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface
            )
            setInteger(MediaFormat.KEY_BIT_RATE, bitrateKbps * 1000)
            setInteger(MediaFormat.KEY_FRAME_RATE, fps)
            setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 2)
            setInteger(
                MediaFormat.KEY_BITRATE_MODE,
                MediaCodecInfo.EncoderCapabilities.BITRATE_MODE_VBR
            )
            // 限制编码器输入帧率：虚拟显示按屏幕刷新率（60/90/120Hz）喂帧，
            // 不限制会输出远超配置的帧率、浪费码率
            if (Build.VERSION.SDK_INT >= 26) {
                setFloat("max-fps-to-encoder", fps.toFloat())
            }
            // 关键帧前重复 SPS/PPS，观看端可随时接入（对应 Rust 端 relay 的对齐逻辑）
            if (Build.VERSION.SDK_INT >= 19) {
                setInteger("prepend-sps-pps-to-idr-frames", 1)
            }
        }

        val codec = MediaCodec.createEncoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
        codec.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
        encoder = codec
        val inputSurface = codec.createInputSurface()
        codec.start()

        virtualDisplay = proj.createVirtualDisplay(
            "stross-display",
            width,
            height,
            host.resources.displayMetrics.densityDpi,
            DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR,
            inputSurface,
            null,
            handler
        )
        Log.i(TAG, "虚拟显示已创建: ${width}x${height}@$fps")
        // 通知 Rust/前端：采集真正就绪
        sendControl(JSObject().apply {
            put("t", 9)
            put("started", true)
            put("fps", fps)
        })

        handler.post { drainVideoLoop(proj) }

        if (withAudio) {
            requestMicAndStart()
        }
    }

    /** 请求麦克风运行时权限；拒绝时纯麦克风模式报错，屏幕模式仅采集屏幕。 */
    private fun requestMicAndStart() {
        if (Build.VERSION.SDK_INT < 23) {
            startAudio()
            return
        }
        val granted = host.checkSelfPermission(Manifest.permission.RECORD_AUDIO) ==
            PackageManager.PERMISSION_GRANTED
        if (granted) {
            startAudio()
        } else {
            PluginManager.requestPermissions(arrayOf(Manifest.permission.RECORD_AUDIO)) { perms ->
                if (perms[Manifest.permission.RECORD_AUDIO] == true) {
                    startAudio()
                } else {
                    Log.w(TAG, "麦克风权限被拒绝，仅采集屏幕")
                    if (micOnly) {
                        failCapture("麦克风权限被拒绝")
                    }
                }
            }
        }
    }

    /** 纯麦克风模式下采集启动失败：回传 t=9 错误帧并复位（无屏幕兜底）。 */
    private fun failCapture(err: String) {
        sendControl(JSObject().apply {
            put("t", 9)
            put("started", false)
            put("err", err)
        })
        running.set(false)
        channel = null
    }

    private fun drainVideoLoop(proj: MediaProjection) {
        val info = MediaCodec.BufferInfo()
        try {
            while (running.get()) {
                val codec = encoder ?: break
                val outIndex = codec.dequeueOutputBuffer(info, 10_000)
                if (outIndex == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED) {
                    continue
                }
                if (outIndex >= 0) {
                    val data = ByteArray(info.size)
                    codec.getOutputBuffer(outIndex)?.get(data)
                    codec.releaseOutputBuffer(outIndex, false)
                    if (info.size <= 0) continue
                    val keyframe = info.flags and MediaCodec.BUFFER_FLAG_KEY_FRAME != 0
                    val config = info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG != 0
                    sendFrame(0, keyframe, config, info.presentationTimeUs / 1000, data)
                }
            }
        } catch (e: Exception) {
            Log.w(TAG, "视频编码循环退出: ${e.message}")
        }
        // 收尾
        try {
            encoder?.stop()
        } catch (_: Exception) {
        }
        try {
            encoder?.release()
        } catch (_: Exception) {
        }
        encoder = null
        virtualDisplay?.release()
        virtualDisplay = null
        proj.stop()
        channel = null
    }

    // ------------------------------------------------------------------
    // 麦克风采集
    // ------------------------------------------------------------------

    private fun startAudio() {
        val sampleRate = 48_000
        val minBuf = AudioRecord.getMinBufferSize(
            sampleRate,
            AudioFormat.CHANNEL_IN_STEREO,
            AudioFormat.ENCODING_PCM_16BIT
        )
        if (minBuf <= 0) {
            Log.w(TAG, "AudioRecord 不支持 48k 立体声")
            if (micOnly) failCapture("麦克风初始化失败（AudioRecord 不可用）")
            return
        }
        val record = AudioRecord(
            MediaRecorder.AudioSource.MIC,
            sampleRate,
            AudioFormat.CHANNEL_IN_STEREO,
            AudioFormat.ENCODING_PCM_16BIT,
            minBuf * 2
        )
        if (record.state != AudioRecord.STATE_INITIALIZED) {
            Log.w(TAG, "AudioRecord 初始化失败")
            if (micOnly) failCapture("麦克风初始化失败")
            return
        }
        audioRecord = record

        val aacFormat = MediaFormat.createAudioFormat(MediaFormat.MIMETYPE_AUDIO_AAC, sampleRate, 2).apply {
            setInteger(MediaFormat.KEY_AAC_PROFILE, MediaCodecInfo.CodecProfileLevel.AACObjectLC)
            setInteger(MediaFormat.KEY_BIT_RATE, 128_000)
            setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, 16 * 1024)
        }
        val codec = MediaCodec.createEncoderByType(MediaFormat.MIMETYPE_AUDIO_AAC)
        codec.configure(aacFormat, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
        audioEncoder = codec
        codec.start()
        record.startRecording()

        audioThread = Thread { drainAudioLoop(record) }.apply { start() }
        // 纯麦克风模式没有屏幕路径的 started 回报，在此补发（t=9 控制帧）
        if (micOnly) {
            sendControl(JSObject().apply {
                put("t", 9)
                put("started", true)
            })
        }
        Log.i(TAG, "麦克风采集启动")
    }

    private fun drainAudioLoop(record: AudioRecord) {
        val info = MediaCodec.BufferInfo()
        try {
            while (running.get()) {
                val codec = audioEncoder ?: break
                val inIdx = codec.dequeueInputBuffer(10_000)
                if (inIdx >= 0) {
                    val inBuf = codec.getInputBuffer(inIdx) ?: continue
                    val n = record.read(inBuf, inBuf.capacity())
                    if (n > 0) {
                        codec.queueInputBuffer(inIdx, 0, n, System.nanoTime() / 1000, 0)
                    } else {
                        codec.queueInputBuffer(inIdx, 0, 0, System.nanoTime() / 1000, MediaCodec.BUFFER_FLAG_END_OF_STREAM)
                        break
                    }
                }
                // 取全部可用输出
                while (true) {
                    val outIdx = codec.dequeueOutputBuffer(info, 0)
                    if (outIdx == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED) {
                        // 首个输出格式里带 AudioSpecificConfig，用于解析 ADTS 头参数
                        parseAacConfig(codec.outputFormat)
                        continue
                    }
                    if (outIdx >= 0) {
                        val data = ByteArray(info.size)
                        codec.getOutputBuffer(outIdx)?.get(data)
                        codec.releaseOutputBuffer(outIdx, false)
                        if (info.size > 0 && info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG == 0) {
                            sendFrame(1, false, false, info.presentationTimeUs / 1000, withAdtsHeader(data))
                        }
                    } else {
                        break
                    }
                }
            }
        } catch (e: Exception) {
            Log.w(TAG, "音频编码循环退出: ${e.message}")
        }
        try {
            record.stop()
        } catch (_: Exception) {
        }
        record.release()
        try {
            audioEncoder?.stop()
        } catch (_: Exception) {
        }
        try {
            audioEncoder?.release()
        } catch (_: Exception) {
        }
        audioRecord = null
        audioEncoder = null
    }

    /** 从 AudioSpecificConfig 解析 ADTS 头参数（profile/freqIdx/channels）。 */
    private fun parseAacConfig(fmt: MediaFormat) {
        val asc = fmt.getByteBuffer("csd-0") ?: return
        val b0 = asc.get(0).toInt() and 0xFF
        val b1 = asc.get(1).toInt() and 0xFF
        aacProfile = (b0 shr 3) and 0x1F
        aacFreqIdx = ((b0 and 0x07) shl 1) or ((b1 shr 7) and 0x01)
        aacChannels = (b1 shr 3) and 0x0F
    }

    /** 给裸 AAC 帧加 7 字节 ADTS 头（观看端 jmuxer 依赖）。 */
    private fun withAdtsHeader(payload: ByteArray): ByteArray {
        val frameLen = payload.size + 7
        val h = ByteArray(7)
        h[0] = 0xFF.toByte()
        h[1] = 0xF1.toByte() // MPEG-4, layer 0, no CRC
        h[2] = (((aacProfile and 0x03) shl 6) or ((aacFreqIdx and 0x0F) shl 2) or ((aacChannels shr 2) and 0x01)).toByte()
        h[3] = (((aacChannels and 0x03) shl 6) or ((frameLen shr 11) and 0x03)).toByte()
        h[4] = ((frameLen shr 3) and 0xFF).toByte()
        h[5] = (((frameLen and 0x07) shl 5) or 0x1F).toByte()
        h[6] = 0xFC.toByte()
        // ByteArray 不支持 + 运算，手工拼接
        val out = ByteArray(frameLen)
        System.arraycopy(h, 0, out, 0, 7)
        System.arraycopy(payload, 0, out, 7, payload.size)
        return out
    }

    // ------------------------------------------------------------------
    // 回传 & 清理
    // ------------------------------------------------------------------

    @Synchronized
    private fun sendFrame(track: Int, keyframe: Boolean, config: Boolean, ptsMs: Long, data: ByteArray) {
        val ch = channel ?: return
        val obj = JSObject()
        obj.put("t", track)
        obj.put("k", keyframe)
        obj.put("c", config)
        obj.put("p", ptsMs)
        obj.put("d", Base64.encodeToString(data, Base64.NO_WRAP))
        ch.send(obj)
    }

    /** 发送控制消息（t=9 表示采集状态事件，Rust 端转发给前端）。 */
    @Synchronized
    private fun sendControl(obj: JSObject) {
        channel?.send(obj)
    }

    private fun stopEverything() {
        if (!running.compareAndSet(true, false)) {
            return
        }
        sendControl(JSObject().apply { put("t", 9); put("stopped", true) })
        channel = null
        host.stopService(Intent(host, ProjectionService::class.java))
        encodeThread?.quitSafely()
        encodeThread = null
        // drain 循环负责 release 编码器与虚拟显示
        try {
            audioRecord?.stop()
        } catch (_: Exception) {
        }
    }
}
