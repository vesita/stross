//! 应用数据目录解析（身份 / 信任清单所在）——全库唯一真源。
//!
//! 分层（docs/layering-architecture.md）：这是**应用策略**（放 stross-app，
//! 不放 stross-core——核心保持零路径约定、平台无关）；壳层（CLI serve /
//! endpoint、GUI）一律调这里，禁止各自再写 XDG/HOME 回退链。
//!
//! GUI 桌面/Android 用自己的 `app_data_dir` 注入（Tauri 生命周期），
//! 不经过本回退链——语义一致：base_dir 由调用方注入，这里只负责 CLI
//! 数据目录的默认解析。

use std::path::PathBuf;

/// 数据目录（identity.json / trusted_devices.json 所在）：`--data-dir` 优先，
/// 否则 XDG_DATA_HOME 或 ~/.local/share/stross（与 GUI 共用同一身份）。
pub fn data_dir(data_dir: Option<PathBuf>) -> PathBuf {
    if let Some(d) = data_dir {
        return d;
    }
    std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(|h| std::path::Path::new(&h).join(".local/share/stross"))
                .unwrap_or_else(|_| PathBuf::from("stross-data"))
        })
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
}
