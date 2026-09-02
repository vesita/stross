//! 应用数据目录解析（identity.json / trusted_devices.json 所在）——唯一真源。
//!
//! 分层：这是**平台适应**（放桥接层，不进内核——内核保持零路径约定、平台无关）；
//! 壳层（CLI serve / GUI 桌面 / Android）一律调这里或注入自己的
//! `app_data_dir`（Tauri 生命周期），禁止各自再写 XDG/HOME 回退链。
//!
//! 语义统一：base_dir 由调用方注入，这里只负责 CLI 数据目录的默认解析。

use std::path::PathBuf;

/// 数据目录（identity.json / trusted_devices.json 所在）：`--data-dir` 优先，
/// 否则按各操作系统原生规范解析（Linux/XDG、macOS Application Support、Windows AppData）。
pub fn data_dir(data_dir: Option<PathBuf>) -> PathBuf {
    if let Some(d) = data_dir {
        return d;
    }

    // 1. 优先读取标准 XDG_DATA_HOME 环境变量（Linux / BSD 等）
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME")
        && !xdg.trim().is_empty()
    {
        let p = PathBuf::from(xdg);
        return if p.ends_with("stross") {
            p
        } else {
            p.join("stross")
        };
    }

    // 2. Windows 原生目录：%LOCALAPPDATA%\stross 或 %APPDATA%\stross
    #[cfg(target_os = "windows")]
    {
        if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
            if !local_app_data.trim().is_empty() {
                return PathBuf::from(local_app_data).join("stross");
            }
        }
        if let Ok(app_data) = std::env::var("APPDATA") {
            if !app_data.trim().is_empty() {
                return PathBuf::from(app_data).join("stross");
            }
        }
    }

    // 3. macOS 原生目录：~/Library/Application Support/stross
    #[cfg(target_os = "macos")]
    {
        if let Ok(home) = std::env::var("HOME") {
            if !home.trim().is_empty() {
                return PathBuf::from(home).join("Library/Application Support/stross");
            }
        }
    }

    // 4. Linux 默认 ~/.local/share/stross 与最终回退
    std::env::var("HOME").map_or_else(
        |_| PathBuf::from("stross-data"),
        |h| std::path::Path::new(&h).join(".local/share/stross"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_dir_wins() {
        assert_eq!(
            data_dir(Some(PathBuf::from("/tmp/stross-a"))),
            PathBuf::from("/tmp/stross-a")
        );
    }

    #[test]
    fn fallback_never_empty() {
        let p = data_dir(None);
        assert!(!p.as_os_str().is_empty());
    }
}
