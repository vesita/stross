package dev.stross.sender

import android.content.Context
import android.graphics.Rect
import android.net.wifi.WifiManager
import android.os.Bundle
import android.util.Log
import android.view.Surface
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import androidx.activity.enableEdgeToEdge
import android.widget.FrameLayout
import java.util.concurrent.atomic.AtomicReference

/**
 * Stross 主 Activity（由 `scripts/setup-android.sh` 复制进生成的 Android 工程）。
 *
 * **原生播放 SurfaceView**（Surface 渲染，替代 WebView-canvas 像素路径）：
 * `MediaCodec` 解码器直接画到这里的 `SurfaceView` 的 Surface，GPU 直出、零像素
 * 搬运。它程序化加到窗口 `DecorView` 顶层（`setZOrderOnTop(true)` 硬件 overlay，
 * 保证盖在 WebView 之上），初始 `GONE`；播放时按前端上报的矩形定位并 `VISIBLE`，
 * 退出时 `GONE`。全屏由原生把 SurfaceView 铺满 + 隐藏系统栏，不走 WebView CSS。
 *
 * **Surface 生命周期**：用 `SurfaceHolder.Callback` 跟踪 surface 创建/销毁——
 * SurfaceView 置 `GONE` 会销毁 surface（媒体解码器因此要等新 surface 重建）。
 * [`PlaybackPlugin`] 经 [`playbackSurface`]/[`isSurfaceReady`] 取当前有效 surface，
 * 在 `surfaceCreated` 后才创建解码器。
 */
class MainActivity : TauriActivity() {
    private var multicastLock: WifiManager.MulticastLock? = null

    /** 原生播放 SurfaceView（视频硬件加速直渲染）。`PlaybackPlugin` 经此取 Surface。 */
    var playbackSurfaceView: SurfaceView? = null
        private set

    /** 当前有效 surface（surfaceCreated 后非空；surfaceDestroyed 后置空）。 */
    private val surfaceRef = AtomicReference<Surface?>()

    /** 最近一次非全屏播放区矩形（物理 px；退出全屏后恢复定位）。 */
    private var lastPlayerRect: Rect? = null

    /** Surface 是否可用（已创建且未销毁）。 */
    fun isSurfaceReady(): Boolean {
        val s = surfaceRef.get() ?: return false
        return try { s.isValid } catch (_: Exception) { false }
    }

    /** 当前有效 surface（可能为 null）。 */
    fun playbackSurface(): Surface? = surfaceRef.get()

    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)
        // mDNS 组播接收：部分 ROM（OPPO ColorOS 等）默认拦截组播包，
        // 必须持有 MulticastLock 才能收到局域网设备发现广播
        // （mdns-sd 无 Android 侧处理；真机实测 browse 收不到 PC 广播）
        try {
            val wifi = getSystemService(Context.WIFI_SERVICE) as WifiManager
            multicastLock = wifi.createMulticastLock("stross-mdns").apply {
                setReferenceCounted(false)
                acquire()
            }
        } catch (_: Exception) {
            // WiFi 服务不可用（无网络）时不致命：mDNS 发现静默失效
        }
        createPlaybackSurface()
    }

    /** 程序化创建 SurfaceView：加到 DecorView 顶层、硬件 overlay、初始 GONE。
     *  surface 经 `SurfaceHolder.Callback` 创建/销毁后同步到 [`surfaceRef`]，
     *  解码器在 [`isSurfaceReady`] 后才配置。 */
    private fun createPlaybackSurface() {
        val sv = SurfaceView(this).apply {
            setZOrderOnTop(true) // 位于窗口内容之上（WebView 顶层）
            setZOrderMediaOverlay(false)
            visibility = View.GONE
            holder.addCallback(object : SurfaceHolder.Callback {
                override fun surfaceCreated(holder: SurfaceHolder) {
                    Log.i("StrossSurface", "surfaceCreated")
                    surfaceRef.set(holder.surface)
                }

                override fun surfaceChanged(
                    holder: SurfaceHolder,
                    format: Int,
                    width: Int,
                    height: Int,
                ) {
                    Log.i("StrossSurface", "surfaceChanged ${width}x$height")
                    surfaceRef.set(holder.surface)
                }

                override fun surfaceDestroyed(holder: SurfaceHolder) {
                    Log.i("StrossSurface", "surfaceDestroyed")
                    surfaceRef.set(null)
                }
            })
        }
        (window.decorView as android.view.ViewGroup).addView(sv, FrameLayout.LayoutParams(1, 1))
        sv.bringToFront()
        playbackSurfaceView = sv
    }

    /** 是否已处于原生全屏（系统栏隐藏）。 */
    var playbackFullscreen = false
        private set

    /** 是否正在显示原生播放 Surface。 */
    fun isPlaybackSurfaceShown(): Boolean {
        val sv = playbackSurfaceView ?: return false
        val lp = sv.layoutParams as? FrameLayout.LayoutParams
        return sv.visibility == View.VISIBLE && (lp?.leftMargin ?: 0) >= 0
    }

    /** 显示 SurfaceView（重置为 1×1 占位不缺省全窗口；前端随后用
     *  [`showPlaybackSurface(rect)`] 重定位）。 */
    fun showPlaybackSurface() {
        val sv = playbackSurfaceView ?: return
        lastPlayerRect?.let { r ->
            showPlaybackSurface(r)
            return
        }
        sv.layoutParams = FrameLayout.LayoutParams(1, 1)
        if (sv.visibility != View.VISIBLE) sv.visibility = View.VISIBLE
        sv.bringToFront()
    }

    /** 播放开始时显示 SurfaceView。若已有最近播放区定位则按该定位显示，
     *  避免无条件全屏盖住整个 UI；无定位时铺满窗口确保 surface 就绪。 */
    fun showPlaybackSurfaceFullWindow() {
        val sv = playbackSurfaceView ?: return
        lastPlayerRect?.let { r ->
            showPlaybackSurface(r)
            return
        }
        sv.layoutParams = FrameLayout.LayoutParams(
            FrameLayout.LayoutParams.MATCH_PARENT,
            FrameLayout.LayoutParams.MATCH_PARENT,
        )
        if (sv.visibility != View.VISIBLE) sv.visibility = View.VISIBLE
        sv.bringToFront()
    }

    /** 显示 SurfaceView 并定位到播放区矩形（物理 px，DecorView 坐标系）。
     *  记录为「最近播放区」，供退出全屏后恢复。 */
    fun showPlaybackSurface(rect: Rect) {
        val sv = playbackSurfaceView ?: return
        val params = FrameLayout.LayoutParams(rect.width(), rect.height()).apply {
            leftMargin = rect.left
            topMargin = rect.top
        }
        sv.layoutParams = params
        if (sv.visibility != View.VISIBLE) sv.visibility = View.VISIBLE
        sv.bringToFront()
        lastPlayerRect = rect
    }

    /** 隐藏 SurfaceView（退出播放 / 切换到共享页等）。
     *  设为 INVISIBLE 并移出视口，既避免销毁底层 Surface 导致解码器崩溃，
     *  又彻底防止遮挡与挤占 WebView 的点击事件与内容画面。 */
    fun hidePlaybackSurface() {
        val sv = playbackSurfaceView ?: return
        sv.visibility = View.INVISIBLE
        val params = FrameLayout.LayoutParams(1, 1).apply {
            leftMargin = -9999
            topMargin = -9999
        }
        sv.layoutParams = params
    }

    /** 原生全屏：SurfaceView 铺满 + 隐藏系统栏（沉浸式）。 */
    fun enterPlaybackFullscreen() {
        val sv = playbackSurfaceView ?: return
        sv.layoutParams = FrameLayout.LayoutParams(
            FrameLayout.LayoutParams.MATCH_PARENT,
            FrameLayout.LayoutParams.MATCH_PARENT,
        )
        if (sv.visibility != View.VISIBLE) sv.visibility = View.VISIBLE
        sv.bringToFront()
        window.decorView.systemUiVisibility = (
            View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
                or View.SYSTEM_UI_FLAG_FULLSCREEN
                or View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
                or View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                or View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
                or View.SYSTEM_UI_FLAG_LAYOUT_STABLE
            )
        playbackFullscreen = true
    }

    /** 退出原生全屏：恢复系统栏；Surface 保持当前布局（前端随后重发播放区矩形）。 */
    fun exitPlaybackFullscreen() {
        window.decorView.systemUiVisibility = View.SYSTEM_UI_FLAG_VISIBLE
        playbackFullscreen = false
    }

    /**
     * 系统返回键：全屏时**优先退出全屏**（保持 activity 存活、surface 有效），
     * 而不是默认行为（finish activity、销毁 surface → 解码器向其渲染崩溃）。
     * 退出后通知前端恢复全屏态（fsActive=false、重定位 surface）。
     */
    override fun onBackPressed() {
        if (playbackFullscreen) {
            exitPlaybackFullscreenForBack()
            return
        }
        super.onBackPressed()
    }

    /** 返回键触发的全屏退出：恢复系统栏 + 定向回 unspecified + 恢复播放区矩形。 */
    private fun exitPlaybackFullscreenForBack() {
        exitPlaybackFullscreen()
        try { requestedOrientation = android.content.pm.ActivityInfo.SCREEN_ORIENTATION_UNSPECIFIED } catch (_: Exception) {}
        lastPlayerRect?.let { showPlaybackSurface(it) } ?: showPlaybackSurfaceFullWindow()
        PlaybackPlugin.instance?.notifyFullscreenExited()
    }

    override fun onDestroy() {
        try {
            multicastLock?.release()
        } catch (_: Exception) {
        }
        multicastLock = null
        super.onDestroy()
    }
}
