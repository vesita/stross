package dev.stross.sender

import android.app.Activity
import android.content.Context
import android.content.Intent
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
import androidx.activity.result.contract.ActivityResultContracts
import androidx.core.content.ContextCompat
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Channel
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.util.concurrent.atomic.AtomicBoolean

/**
 * Stross Android 采集插件。
 *
 * - 屏幕：MediaProjection 授权 → 前台服务 → VirtualDisplay 直连 MediaCodec(H.264) 编码器输入面
 * - 麦克风：AudioRecord → MediaCodec(AAC) → 手动加 ADTS 头
 *
 * 编码后的帧通过 Tauri [Channel] 回传 Rust（base64 JSON，见 `mobile.rs`）。
 *
 * 依赖本机实现：MediaProjection 的授权对话框、前台服务（API 34+ 强制要求
 * `FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION`）均由本插件完成。
 */
@TauriPlugin
class MediaPlugin(private val activity: Activity) : Plugin(activity) {

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
    }

    private var channel: Channel? = null
    private val running = AtomicBoolean(false)

    // 编码参数
    private var width = 1280
    private var height = 720
    private var fps = 30
    private var bitrateKbps = 2500
    private var withAudio = true

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

    // 屏幕录制授权（需要用户确认对话框）
    private val projectionLauncher = activity.registerForActivityResult(
        ActivityResultContracts.StartActivityForResult()
    ) { result ->
        val invoke = pendingInvoke
        pendingInvoke = null
        if (result.resultCode == Activity.RESULT_OK && result.data != null) {
            startProjectionService(result.resultCode, result.data!!)
            invoke?.resolve(JSObject().apply { put("started", true) })
        } else {
            invoke?.reject("用户拒绝了屏幕录制授权")
        }
    }

    // 麦克风运行时权限（拒绝则仅采集屏幕）
    private val audioPermissionLauncher = activity.registerForActivityResult(
        ActivityResultContracts.RequestPermission()
    ) { granted ->
        if (granted && withAudio) {
            startAudio()
        } else if (!granted) {
            Log.w(TAG, "麦克风权限被拒绝，仅采集屏幕")
        }
    }

    // ------------------------------------------------------------------
    // 命令
    // ------------------------------------------------------------------

    @Command
    fun startCapture(invoke: Invoke) {
        val args = invoke.parseArgs(CaptureArgs())
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
        withAudio = args.withAudio

        pendingInvoke = invoke
        val pm = activity.getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
        projectionLauncher.launch(pm.createScreenCaptureIntent())
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
        val intent = Intent(activity, ProjectionService::class.java).apply {
            putExtra(ProjectionService.EXTRA_RESULT_CODE, resultCode)
            putExtra(ProjectionService.EXTRA_RESULT_DATA, data)
        }
        ContextCompat.startForegroundService(activity, intent)
        val projection = ProjectionService.awaitProjection(10_000)
        if (projection == null) {
            Log.e(TAG, "获取 MediaProjection 超时")
            channel = null
            return
        }
        startProjection(projection)
    }

    private fun startProjection(proj: MediaProjection) {
        if (!running.compareAndSet(false, true)) {
            return
        }
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

        val dm = activity.getSystemService(Context.DISPLAY_SERVICE) as DisplayManager
        virtualDisplay = proj.createVirtualDisplay(
            "stross-display",
            width,
            height,
            activity.resources.displayMetrics.densityDpi,
            DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR,
            inputSurface,
            null,
            handler
        )
        Log.i(TAG, "虚拟显示已创建: ${width}x${height}@$fps")

        handler.post { drainVideoLoop(proj) }

        if (withAudio) {
            if (Build.VERSION.SDK_INT >= 23) {
                val granted = activity.checkSelfPermission(android.Manifest.permission.RECORD_AUDIO)
                    == android.content.pm.PackageManager.PERMISSION_GRANTED
                if (granted) startAudio() else audioPermissionLauncher.launch(android.Manifest.permission.RECORD_AUDIO)
            } else {
                startAudio()
            }
        }
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
                        val fmt = codec.outputFormat
                        parseAacConfig(fmt)
                        continue
                    }
                    if (outIdx >= 0) {
                        val data = ByteArray(info.size)
                        codec.getOutputBuffer(outIdx)?.get(data)
                        codec.releaseOutputBuffer(outIdx, false)
                        if (info.size > 0 && info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG == 0) {
                            val adts = withAdtsHeader(data)
                            sendFrame(1, false, false, info.presentationTimeUs / 1000, adts)
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
        return h + payload
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

    private fun stopEverything() {
        if (!running.compareAndSet(true, false)) {
            return
        }
        channel = null
        activity.stopService(Intent(activity, ProjectionService::class.java))
        encodeThread?.quitSafely()
        encodeThread = null
        // drain 循环负责 release 编码器与虚拟显示
        try {
            audioRecord?.stop()
        } catch (_: Exception) {
        }
    }
}
