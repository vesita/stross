//! 文件端点本地文件源（v3 §7 模块拆分：`kernel/endpoint/` 目录）。

use std::path::PathBuf;

/// 文件端点本地文件源（`control.rs` 状态展示用；路径不落 wire）。
#[derive(Debug, Clone)]
pub struct FileSource {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
}
