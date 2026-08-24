package dev.stross.sender

import android.app.Activity
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioTrack
import android.media.MediaCodec
import android.media.MediaFormat
import android.util.Base64
import android.util.Log
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Channel
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.nio.ByteBuffer
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean

/**
 * Stross Android 播放插件（1f-3）：与采集侧对称的"帧 → 设备"路径。
 *
 * 由 Rust 侧 `register_android_plugin` 自动实例化（构造签名必须为 `(Activity)`）。
 *
 * - 视频：`feedVideo` 喂 Annex-B H.264 帧 → MediaCodec 解码 → YUV420 → RGBA →
 *   最近邻缩放到 ≤480 宽 → Channel 回传 Rust（`receive-frame` 事件 → 前端 canvas）
 * - 音频：`feedAudio` 喂 ADTS AAC → MediaCodec 解码 → PCM → AudioTrack 播放
 *   （队列线程写设备，避免音频满阻塞视频链路）
 * - `startPlayback` / `stopPlayback`：生命周期（AudioTrack 可选，`audio=false`
 *   时 Rust 侧根本不喂音频帧，Kotlin 不建 AudioTrack）
 *
 * 帧消息格式（Rust → Kotlin，JSON）：`{"d": "<base64>", "k": bool, "c": bool, "p": pts_ms}`
 * 回传（Kotlin → Rust）：`{"w": int, "h": int, "pts": int, "d": "<base64 RGBA>"}`
 */
@TauriPlugin
class PlaybackPlugin(activity: Activity) : Plugin(activity) {

    companion object {
        private const val TAG = "StrossPlay"
        /** 回传画面最大宽度（与桌面 scale_rgba 一致，控制 IPC 流量）。 */
        private const val MAX_FRAME_W = 480
        private const val COLOR_FORMAT_YUV420_PLANAR = 19
        private const val COLOR_FORMAT_YUV420_SEMIPLANAR = 21
    }

    @InvokeArg
    class StartArgs {
        var audio: Boolean = false
        var channel: Channel? = null
    }

    @InvokeArg
    class FeedArgs {
        var d: String = "" // base64
        var k: Boolean = false
        var c: Boolean = false
        var p: Long = 0
    }

    private var channel: Channel? = null
    private val running = AtomicBoolean(false)

    // 视频解码
    private var vDecoder: MediaCodec? = null
    private var vWidth = 0
    private var vHeight = 0
    private var vColorFormat = COLOR_FORMAT_YUV420_SEMIPLANAR
    private var vStrideY = 0
    private var vSliceH = 0

    // 音频
    private var aDecoder: MediaCodec? = null
    private var audioTrack: AudioTrack? = null
    private val audioQueue = LinkedBlockingQueue<ByteArray>()
    private var audioThread: Thread? = null
    private var audioSampleRate = 48_000
    private var audioChannels = 2

    // ------------------------------------------------------------------
    // 生命周期
    // ------------------------------------------------------------------

    @Command
    fun startPlayback(invoke: Invoke) {
        val args = invoke.parseArgs(StartArgs::class.java)
        channel = args.channel
        running.set(true)
        if (args.audio) {
            startAudioTrack()
        }
        invoke.resolve(JSObject().apply { put("started", true) })
    }

    @Command
    fun stopPlayback(invoke: Invoke) {
        if (running.compareAndSet(true, false)) {
            stopEverything()
        }
        invoke.resolve(JSObject().apply { put("stopped", true) })
    }

    // ------------------------------------------------------------------
    // 视频：Annex-B → MediaCodec → RGBA（缩放）→ Channel
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
            if (args.c) {
                // 独立配置帧（SPS/PPS）：建（或重建）解码器
                createVideoDecoder(bytes)
            } else {
                if (vDecoder == null) {
                    // 协议流不单独发 config 帧：SPS/PPS 内嵌在关键帧的 Annex-B
                    // 载荷里（发送侧 AccessUnit 组装），从中提取 csd 建解码器
                    val csd = extractSpsPps(bytes)
                    if (csd == null) {
                        invoke.resolve(JSObject())
                        return // 首个含 SPS 的关键帧未到：丢弃等下一帧
                    }
                    createVideoDecoder(csd)
                }
                decodeVideoFrame(bytes, args.p)
            }
        } catch (e: Exception) {
            Log.w(TAG, "feedVideo 失败: ${e.message}")
        }
        invoke.resolve(JSObject())
    }

    /** 从 Annex-B 访问单元提取 SPS(+PPS) 的 NAL 内容（无起始码）作 csd-0。 */
    private fun extractSpsPps(au: ByteArray): ByteArray? {
        val nals = ArrayList<ByteArray>()
        var i = 0
        while (i + 3 < au.size) {
            val sc = when {
                au[i].toInt() == 0 && au[i + 1].toInt() == 0 && au[i + 2].toInt() == 1 -> 3
                i + 4 <= au.size && au[i].toInt() == 0 && au[i + 1].toInt() == 0 &&
                    au[i + 2].toInt() == 0 && au[i + 3].toInt() == 1 -> 4
                else -> 0
            }
            if (sc == 0) {
                i++
                continue
            }
            val hdr = i + sc
            if (hdr >= au.size) break
            val nalType = au[hdr].toInt() and 0x1F
            if (nalType == 7 || nalType == 8) {
                // 该 NAL 结束于下一个起始码（或帧尾）
                var end = au.size
                var k = hdr
                while (k + 2 < au.size) {
                    val nextSc = (au[k].toInt() == 0 && au[k + 1].toInt() == 0 && au[k + 2].toInt() == 1) ||
                        (k + 3 < au.size && au[k].toInt() == 0 && au[k + 1].toInt() == 0 &&
                            au[k + 2].toInt() == 0 && au[k + 3].toInt() == 1)
                    if (nextSc) {
                        end = k
                        break
                    }
                    k++
                }
                nals.add(au.copyOfRange(hdr, end))
                i = end
                if (nalType == 8) break // SPS+PPS 已齐
            } else if (nals.isNotEmpty()) {
                break // 已收集 SPS，后续是 IDR 等：停止
            } else {
                i += sc
            }
        }
        if (nals.isEmpty()) return null
        val total = nals.sumOf { it.size }
        val out = ByteArray(total)
        var off = 0
        for (n in nals) {
            System.arraycopy(n, 0, out, off, n.size)
            off += n.size
        }
        return out
    }

    /** 从 Annex-B 配置帧解析 SPS 尺寸并创建 AVC 解码器（csd-0 = SPS/PPS）。 */
    private fun createVideoDecoder(csd: ByteArray) {
        releaseVideoDecoder()
        val dims = parseSpsDimensions(csd) ?: (1280 to 720)
        val fmt = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, dims.first, dims.second).apply {
            setByteBuffer("csd-0", ByteBuffer.wrap(csd))
            setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, 2 * 1024 * 1024)
        }
        val codec = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
        codec.configure(fmt, null, null, 0)
        codec.start()
        vDecoder = codec
        vWidth = 0
        vHeight = 0
        Log.i(TAG, "视频解码器就绪（SPS ${dims.first}x${dims.second}）")
    }

    private fun decodeVideoFrame(data: ByteArray, ptsMs: Long) {
        val codec = vDecoder ?: return // 尚未收到 SPS/PPS：丢弃（等配置帧）
        val inIdx = codec.dequeueInputBuffer(5_000)
        if (inIdx < 0) return
        val inBuf = codec.getInputBuffer(inIdx) ?: return
        inBuf.clear()
        inBuf.put(data)
        codec.queueInputBuffer(inIdx, 0, data.size, ptsMs * 1000, 0)
        drainVideoOutput(codec)
    }

    private fun drainVideoOutput(codec: MediaCodec) {
        val info = MediaCodec.BufferInfo()
        while (true) {
            val idx = codec.dequeueOutputBuffer(info, 0)
            when {
                idx == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED -> {
                    val fmt = codec.outputFormat
                    vWidth = fmt.getInteger(MediaFormat.KEY_WIDTH)
                    vHeight = fmt.getInteger(MediaFormat.KEY_HEIGHT)
                    vColorFormat = fmt.getInteger(MediaFormat.KEY_COLOR_FORMAT)
                    // stride / slice-height（API 23+；缺失则按紧凑布局）
                    vStrideY = if (fmt.containsKey(MediaFormat.KEY_STRIDE)) fmt.getInteger(MediaFormat.KEY_STRIDE) else vWidth
                    vSliceH = if (fmt.containsKey(MediaFormat.KEY_SLICE_HEIGHT)) fmt.getInteger(MediaFormat.KEY_SLICE_HEIGHT) else vHeight
                }
                idx >= 0 -> {
                    val size = info.size
                    val flags = info.flags
                    if (size > 0 && flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG == 0 && vWidth > 0) {
                        val buf = codec.getOutputBuffer(idx) ?: continue
                        buf.position(info.offset)
                        buf.limit(info.offset + size)
                        sendRgbaFrame(buf, vWidth, vHeight, vColorFormat, info.presentationTimeUs / 1000, info.offset)
                    }
                    codec.releaseOutputBuffer(idx, false)
                }
                else -> return // 无更多输出
            }
        }
    }

    /** YUV420 → RGBA（最近邻缩放到 ≤480 宽）→ Channel 回传。 */
    private fun sendRgbaFrame(
        buf: ByteBuffer,
        w: Int,
        h: Int,
        colorFormat: Int,
        ptsMs: Long,
        offset: Int,
    ) {
        val strideY = if (vStrideY > 0) vStrideY else w
        val sliceH = if (vSliceH > 0) vSliceH else h
        // 缩放目标尺寸（保持宽高比，与桌面 scale_rgba 相同算法）
        val tw = minOf(w, MAX_FRAME_W)
        val th = maxOf(1, h * tw / w)
        val out = ByteArray(tw * th * 4)
        val planeSize = strideY * sliceH
        val uvStart = planeSize
        val uvStride = if (colorFormat == COLOR_FORMAT_YUV420_PLANAR) strideY / 2 else strideY

        for (y in 0 until th) {
            val sy = y * h / th
            for (x in 0 until tw) {
                val sx = x * w / tw
                val yIdx = offset + sy * strideY + sx
                val yv = buf.get(yIdx).toInt() and 0xFF
                val uIdx = offset + uvStart + (sy / 2) * uvStride + (sx / 2)
                val uv = if (colorFormat == COLOR_FORMAT_YUV420_PLANAR) {
                    // I420：U 平面、V 平面分开
                    val u = buf.get(uIdx).toInt() and 0xFF
                    val v = buf.get(uIdx + planeSize / 4).toInt() and 0xFF
                    u to v
                } else {
                    // NV12：UV 交错（U 在前）
                    val u = buf.get(uIdx).toInt() and 0xFF
                    val v = buf.get(uIdx + 1).toInt() and 0xFF
                    u to v
                }
                val c = yv - 16
                val d = uv.first - 128
                val e = uv.second - 128
                val r = clampByte((298 * c + 409 * e + 128) shr 8)
                val g = clampByte((298 * c - 100 * d - 208 * e + 128) shr 8)
                val b = clampByte((298 * c + 516 * d + 128) shr 8)
                val o = (y * tw + x) * 4
                out[o] = r
                out[o + 1] = g
                out[o + 2] = b
                out[o + 3] = 255.toByte()
            }
        }
        channel?.send(JSObject().apply {
            put("w", tw)
            put("h", th)
            put("pts", ptsMs)
            put("d", Base64.encodeToString(out, Base64.NO_WRAP))
        })
    }

    private fun clampByte(v: Int): Byte {
        val c = if (v < 0) 0 else if (v > 255) 255 else v
        return c.toByte()
    }

    // ------------------------------------------------------------------
    // 音频：ADTS → raw AAC → MediaCodec → PCM → AudioTrack（队列线程）
    // ------------------------------------------------------------------

    private fun startAudioTrack() {
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
            .build()
        audioTrack = track
        track.play()
        audioThread = Thread { audioWriteLoop() }.apply { start() }
        Log.i(TAG, "AudioTrack 就绪")
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
        val inIdx = codec.dequeueInputBuffer(5_000)
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

    // ------------------------------------------------------------------
    // SPS 解析（H.264 序列参数集 → 宽高）
    // ------------------------------------------------------------------

    /** 从 Annex-B 配置帧里找 SPS（NAL type 7），解析宽高；找不到返回 null。 */
    private fun parseSpsDimensions(csd: ByteArray): Pair<Int, Int>? {
        var i = 0
        while (i + 3 < csd.size) {
            if ((csd[i].toInt() and 0xFF) == 0 && (csd[i + 1].toInt() and 0xFF) == 0
                && (csd[i + 2].toInt() and 0xFF) == 1
            ) {
                val nalType = (csd[i + 3].toInt() and 0x1F)
                if (nalType == 7) {
                    return parseSps(csd, i + 4)
                }
                i += 3
            } else {
                i++
            }
        }
        return null
    }

    /** 解析 SPS 载荷（跳过 NAL header 后），返回 (宽, 高)。 */
    private fun parseSps(sps: ByteArray, start: Int): Pair<Int, Int>? {
        if (start >= sps.size) return null
        val br = BitReader(sps, start)
        val profileIdc = br.readBits(8)
        br.readBits(8) // constraint flags + reserved
        br.readBits(8) // level_idc
        br.readUE() // seq_parameter_set_id
        if (isHighProfile(profileIdc)) {
            val chromaFormat = br.readUE()
            if (chromaFormat == 3) br.readBits(1) // separate_colour_plane_flag
            br.readUE() // bit_depth_luma_minus8
            br.readUE() // bit_depth_chroma_minus8
            br.readBits(1) // qpprime_y_zero_transform_bypass_flag
            if (br.readBits(1) == 1) {
                // seq_scaling_matrix_present_flag：跳过 scaling lists
                val n = if (chromaFormat != 3) 8 else 12
                for (list in 0 until n) {
                    if (br.readBits(1) == 1) {
                        val size = if (list < 6) 16 else 64
                        skipScalingList(br, size)
                    }
                }
            }
        }
        br.readUE() // log2_max_frame_num_minus4
        val pocType = br.readUE()
        if (pocType == 0) {
            br.readUE() // log2_max_pic_order_cnt_lsb_minus4
        } else if (pocType == 1) {
            br.readBits(1) // delta_pic_order_always_zero_flag
            br.readSE() // offset_for_non_ref_pic
            br.readSE() // offset_for_top_to_bottom_field
            val n = br.readUE()
            for (i in 0 until n) br.readSE() // offset_for_ref_frame
        }
        br.readUE() // max_num_ref_frames
        br.readBits(1) // gaps_in_frame_num_value_allowed_flag
        val wMbs = br.readUE() + 1
        val hMapUnits = br.readUE() + 1
        val frameMbsOnly = br.readBits(1)
        val height = hMapUnits * 16 * (2 - frameMbsOnly)
        val width = wMbs * 16
        if (width <= 0 || height <= 0 || width > 4096 || height > 4096) return null
        return width to height
    }

    private fun isHighProfile(p: Int): Boolean = p == 100 || p == 110 || p == 122 || p == 244 ||
        p == 44 || p == 83 || p == 86 || p == 118 || p == 128 || p == 138 ||
        p == 139 || p == 134 || p == 135

    /** 跳过一组 scaling list（仅 delta_scale 序列，不建表）。 */
    private fun skipScalingList(br: BitReader, size: Int) {
        var lastScale = 8
        var nextScale = 8
        for (j in 0 until size) {
            if (nextScale != 0) {
                val delta = br.readSE()
                nextScale = (lastScale + delta + 256) % 256
            }
            if (nextScale != 0) lastScale = nextScale
        }
    }

    /** 逐位读取器（含 ue/se 哥伦布）。 */
    private class BitReader(private val data: ByteArray, private var pos: Int) {
        private var bitPos = 0

        fun readBits(n: Int): Int {
            var v = 0
            for (i in 0 until n) {
                v = (v shl 1) or readBit()
            }
            return v
        }

        private fun readBit(): Int {
            if (pos >= data.size) return 0
            val b = (data[pos].toInt() and 0xFF) shr (7 - bitPos) and 1
            bitPos++
            if (bitPos == 8) {
                bitPos = 0
                pos++
            }
            return b
        }

        /** 无符号指数哥伦布（Exp-Golomb）。 */
        fun readUE(): Int {
            var zeros = 0
            while (readBit() == 0) zeros++
            if (zeros == 0) return 0
            return (1 shl zeros) - 1 + readBits(zeros)
        }

        /** 有符号指数哥伦布。 */
        fun readSE(): Int {
            val ue = readUE()
            if (ue == 0) return 0
            return if (ue % 2 == 0) -(ue / 2) else (ue + 1) / 2
        }
    }

    // ------------------------------------------------------------------
    // 清理
    // ------------------------------------------------------------------

    private fun stopEverything() {
        releaseVideoDecoder()
        channel = null
        // 音频：清队 + 停线程 + 释放
        audioQueue.clear()
        audioThread?.interrupt()
        audioThread = null
        try {
            aDecoder?.stop()
        } catch (_: Exception) {
        }
        try {
            aDecoder?.release()
        } catch (_: Exception) {
        }
        aDecoder = null
        try {
            audioTrack?.stop()
        } catch (_: Exception) {
        }
        try {
            audioTrack?.release()
        } catch (_: Exception) {
        }
        audioTrack = null
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
        vStrideY = 0
        vSliceH = 0
    }
}
