package dev.stross.sender

import android.os.Bundle
import app.tauri.activity.TauriActivity
import app.tauri.plugin.PluginManager

/**
 * Stross 主 Activity（由 `scripts/setup-android.sh` 复制进生成的 Android 工程）。
 */
class MainActivity : TauriActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // 注册原生采集插件
        PluginManager.get().addPlugin(MediaPlugin(this))
    }
}
