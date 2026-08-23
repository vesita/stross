// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    #[cfg(not(mobile))]
    {
        // PC 端整合：同一二进制支持两种模式——
        //   stross-sender                桌面应用（连接→推流/观看，内嵌中继）
        //   stross-sender --relay-only   无界面中继（服务器/常驻部署）
        let args: Vec<String> = std::env::args().collect();
        if args.iter().any(|a| a == "--relay-only") {
            stross_sender_lib::run_relay_only(&args);
            return;
        }
    }
    stross_sender_lib::run()
}
