package dev.stross.sender

import android.content.Context
import android.net.wifi.WifiManager
import android.os.Bundle
import androidx.activity.enableEdgeToEdge

/**
 * Stross 主 Activity（由 `scripts/setup-android.sh` 复制进生成的 Android 工程）。
 *
 * 注意：`TauriActivity` 由 tauri-codegen 自动生成到本包名下；
 * 原生插件由 Rust 侧 `register_android_plugin` 自动实例化并注册，无需在此手动添加。
 */
class MainActivity : TauriActivity() {
    private var multicastLock: WifiManager.MulticastLock? = null

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