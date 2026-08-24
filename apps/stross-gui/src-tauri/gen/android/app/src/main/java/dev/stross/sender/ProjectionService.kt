package dev.stross.sender

import android.app.Activity
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.hardware.display.DisplayManager
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.util.Log

/**
 * MediaProjection 前台服务。
 *
 * Android 14（API 34+）强制要求：`getMediaProjection()` 必须在已启动的
 * `mediaProjection` 类型前台服务上下文中调用，否则抛 SecurityException。
 *
 * 流程：插件把授权结果（resultCode + data）交给本服务 → 本服务启动前台
 * 通知 → 获取 [MediaProjection] → 通过 [awaitProjection] 交给插件。
 */
class ProjectionService : Service() {

    companion object {
        private const val TAG = "StrossProjection"
        private const val CHANNEL_ID = "stross_projection"
        private const val NOTIFICATION_ID = 1

        const val EXTRA_RESULT_CODE = "resultCode"
        const val EXTRA_RESULT_DATA = "resultData"

        // 每次推流可复用：`ready` 标记 + 条件等待（CountDownLatch 一次性，
        // 第二次推流会立即返回旧状态导致"伪超时"）。
        private val lock = Object()

        @Volatile
        private var projection: MediaProjection? = null
        private var ready = false

        /** 重置等待状态（插件每次请求投影前调用）。 */
        fun resetProjection() {
            synchronized(lock) {
                projection = null
                ready = false
            }
        }

        /** 等待插件取走 MediaProjection（最多 [timeoutMs] 毫秒）。 */
        fun awaitProjection(timeoutMs: Long): MediaProjection? {
            synchronized(lock) {
                if (!ready) {
                    try {
                        lock.wait(timeoutMs)
                    } catch (e: InterruptedException) {
                        return null
                    }
                }
                return projection
            }
        }

        /** 服务侧投递投影（成功或失败都通知等待方）。 */
        fun provideProjection(p: MediaProjection?) {
            synchronized(lock) {
                projection = p
                ready = true
                lock.notifyAll()
            }
        }

        fun consumeProjection(): MediaProjection? {
            synchronized(lock) {
                val p = projection
                projection = null
                return p
            }
        }
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        startAsForeground()
        // 幂等：已有投影时直接返回，避免重复 getMediaProjection。
        // 关键：第二次 getMediaProjection 会使第一次的 token 失效，
        // 再用旧 token createVirtualDisplay 会抛
        // "Cannot create VirtualDisplay with non-current MediaProjection"。
        // （OPPO 等 ROM 会把前台服务启动延迟并重复回调 onStartCommand。）
        if (projection != null) {
            return START_NOT_STICKY
        }
        val code = intent?.getIntExtra(EXTRA_RESULT_CODE, Activity.RESULT_CANCELED)
            ?: Activity.RESULT_CANCELED
        val data = intent?.getParcelableExtra<Intent>(EXTRA_RESULT_DATA)

        if (code == Activity.RESULT_OK && data != null) {
            val pm = getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
            val proj = try {
                pm.getMediaProjection(code, data)
            } catch (e: Exception) {
                Log.w(TAG, "getMediaProjection 失败: ${e.message}")
                null
            }
            if (proj != null) {
                proj.registerCallback(object : MediaProjection.Callback() {
                    override fun onStop() {
                        stopSelf()
                    }
                }, Handler(Looper.getMainLooper()))
                provideProjection(proj)
            } else {
                provideProjection(null)
            }
        } else {
            provideProjection(null)
        }
        return START_NOT_STICKY
    }

    private fun startAsForeground() {
        val nm = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            nm.createNotificationChannel(
                NotificationChannel(CHANNEL_ID, "屏幕录制", NotificationManager.IMPORTANCE_LOW)
            )
        }
        val notification: Notification =
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                Notification.Builder(this, CHANNEL_ID)
                    .setContentTitle("Stross")
                    .setContentText("正在录制屏幕并推流到局域网")
                    .setSmallIcon(android.R.drawable.ic_menu_camera)
                    .build()
            } else {
                @Suppress("DEPRECATION")
                Notification.Builder(this)
                    .setContentTitle("Stross")
                    .setContentText("正在录制屏幕并推流到局域网")
                    .setSmallIcon(android.R.drawable.ic_menu_camera)
                    .build()
            }

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(NOTIFICATION_ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION)
        } else {
            startForeground(NOTIFICATION_ID, notification)
        }
    }

    override fun onDestroy() {
        super.onDestroy()
        try {
            consumeProjection()?.stop()
        } catch (_: Exception) {
        }
    }
}
