package dev.stross.sender

import android.content.Context
import android.graphics.Rect
import android.net.wifi.WifiManager
import android.os.Bundle
import android.view.SurfaceView
import android.view.View
import androidx.activity.enableEdgeToEdge
import android.widget.FrameLayout

/**
 * Stross 主 Activity（由 `scripts/setup-android.sh` 复制进生成的 Android 工程）。
 *
 * 注意：`TauriActivity` 由 tauri-codegen 自动生成到本包名下；
 * 原生插件由 Rust 侧 `register_android_plugin` 自动实例化并注册，无需在此手动添加。
 *
 * **原生播放 SurfaceView**（Surface 渲染，替代 WebView-canvas 像素路径）：
 * `MediaCodec` 解码器直接画到这里的 `SurfaceView` 的 Surface，GPU 直出、零像素
 * 搬运。它程序化加到窗口 `DecorView` 顶层（`setZOrderOnTop(true)` 硬件 overlay，
 * 保证盖在 WebView 之上），初始 `GONE`；播放时按前端上报的矩形定位并 `VISIBLE`，
 * 退出时 `GONE`。全屏由原生把 SurfaceView 铺满 + 隐藏系统栏，不走 WebView CSS。
 */
class MainActivity : TauriActivity() {
    private var multicastLock: WifiManager.MulticastLock? = null

    /** 原生播放 SurfaceView（视频硬件加速直渲染）。`PlaybackPlugin` 经此取 Surface。 */
    var playbackSurfaceView: SurfaceView? = null
        private set

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

    /** 程序化创建 SurfaceView：加到 DecorView 顶层、硬件 overlay、小占位（保持
     *  surface 始终有效，供 MediaCodec 随时配置；前端随后按播放区矩形重定位）。 */
    private fun createPlaybackSurface() {
        val sv = SurfaceView(this).apply {
            setZOrderOnTop(true) // 位于窗口内容之上（WebView 顶层）
            setZOrderMediaOverlay(false)
        }
        // 用 FrameLayout 布局参数：初始 1×1 占位（GONE 会销毁 surface，故用 VISIBLE
        // + 极小尺寸保持在角上不可见，始终提供有效 surface）
        (window.decorView as android.view.ViewGroup).addView(sv, FrameLayout.LayoutParams(1, 1))
        sv.bringToFront()
        playbackSurfaceView = sv
    }

    /** 是否已处于原生全屏（系统栏隐藏）。 */
    var playbackFullscreen = false
        private set

    /** 是否正在显示原生播放 Surface。 */
    fun isPlaybackSurfaceShown(): Boolean {
        return playbackSurfaceView?.visibility == View.VISIBLE
    }

    /** 显示 SurfaceView（重置为 1×1 占位不缺省全窗口；前端随后用
     *  [`showPlaybackSurface(rect)`] 重定位）。 */
    fun showPlaybackSurface() {
        val sv = playbackSurfaceView ?: return
        sv.layoutParams = FrameLayout.LayoutParams(1, 1)
        if (sv.visibility != View.VISIBLE) sv.visibility = View.VISIBLE
        sv.bringToFront()
    }

    /** 显示 SurfaceView 并定位到播放区矩形（物理 px，DecorView 坐标系）。 */
    fun showPlaybackSurface(rect: Rect) {
        val sv = playbackSurfaceView ?: return
        val params = FrameLayout.LayoutParams(rect.width(), rect.height()).apply {
            leftMargin = rect.left
            topMargin = rect.top
        }
        sv.layoutParams = params
        if (sv.visibility != View.VISIBLE) sv.visibility = View.VISIBLE
        sv.bringToFront()
    }

    /** 隐藏 SurfaceView（退出播放 / 暂停）。 */
    fun hidePlaybackSurface() {
        playbackSurfaceView?.visibility = View.GONE
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

    override fun onDestroy() {
        try {
            multicastLock?.release()
        } catch (_: Exception) {
        }
        multicastLock = null
        super.onDestroy()
    }
}
