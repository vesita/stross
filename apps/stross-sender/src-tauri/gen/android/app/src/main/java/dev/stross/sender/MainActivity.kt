package dev.stross.sender

import android.os.Bundle
import androidx.activity.enableEdgeToEdge

/**
 * Stross 主 Activity（由 `scripts/setup-android.sh` 复制进生成的 Android 工程）。
 *
 * 注意：`TauriActivity` 由 tauri-codegen 自动生成到本包名下；
 * 原生插件由 Rust 侧 `register_android_plugin` 自动实例化并注册，无需在此手动添加。
 */
class MainActivity : TauriActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)
    }
}
