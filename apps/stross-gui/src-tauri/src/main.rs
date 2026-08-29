// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    #[cfg(not(mobile))]
    {
        // PC 端整合：同一二进制支持两种模式——
        //   stross-gui                桌面应用（连接→推流/观看，内嵌中继）
        //   stross-gui --relay-only   无界面中继（服务器/常驻部署）
        let args: Vec<String> = std::env::args().collect();
        if args.iter().any(|a| a == "--relay-only") {
            stross_gui_lib::run_relay_only(&args);
            return;
        }
    }
    #[cfg(all(not(mobile), target_os = "linux"))]
    apply_linux_webkit_workarounds();
    stross_gui_lib::run();
}

/// NVIDIA 闭源驱动 + Wayland 下，webkit2gtk 的 DMA-BUF 渲染器与合成器
/// 协商失败（`Gdk-Message: Error 71 (协议错误) dispatching to Wayland display`），
/// 启动早期关闭该渲染器、回退到共享内存合成。
///
/// 仅当「Wayland 会话 + NVIDIA 驱动已加载」且用户未显式设置该变量时生效，
/// 不干扰 X11/其它 GPU；用户可自行用 `WEBKIT_DISABLE_DMABUF_RENDERER` 覆盖。
#[cfg(all(not(mobile), target_os = "linux"))]
fn apply_linux_webkit_workarounds() {
    let on_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let backend_forced_x11 = std::env::var_os("GDK_BACKEND").is_some_and(|v| v == "x11");
    let nvidia_loaded = std::path::Path::new("/proc/driver/nvidia/version").exists();
    let user_override = std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_some();
    if on_wayland && !backend_forced_x11 && nvidia_loaded && !user_override {
        // SAFETY: 进程启动早期、创建任何线程与 WebKitWebContext（首个 WebView）
        // 之前设置，无并发读取该环境变量的竞态；webkit 初始化时才读取它。
        unsafe {
            std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        }
    }
}
