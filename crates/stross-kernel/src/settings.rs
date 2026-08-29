//! 应用设置持久化（`settings.json`）——单一真源，`base_dir` 由上层注入。
//!
//! 与 identity.json / trusted_devices.json 同目录。当前仅一个设置项：
//! `discoverable`（**可被发现**，mDNS 广播本机）。默认**关闭**——用户显式
//! 开启才被局域网扫描发现（隐私优先）。
//!
//! 分层：内核不解析路径（base_dir 注入），不做平台约定；JSON 读写收敛在此，
//! 壳层（CLI / GUI）经 [`load_or_default`] 注入启动值、经 [`save`] 持久化开关。

use std::path::Path;

use serde::{Deserialize, Serialize};

/// 设置文件名（与 identity.json / trusted_devices.json 同目录）。
pub const SETTINGS_FILE: &str = "settings.json";

/// 应用设置（当前仅「可被发现」一项）。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// 是否可被发现（mDNS 广播本机）。默认 `false`（隐私优先）。
    pub discoverable: bool,
}

/// 从 `base_dir` 加载设置（不存在 / 坏 JSON → 默认值；与 identity 同语义）。
pub fn load_or_default(base_dir: &Path) -> Settings {
    let path = base_dir.join(SETTINGS_FILE);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<Settings>(&s).ok())
        .unwrap_or_default()
}

/// 持久化设置到 `base_dir`（写坏 / 目录建失败仅告警，不中断调用方）。
pub fn save(base_dir: &Path, settings: &Settings) {
    let path = base_dir.join(SETTINGS_FILE);
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        tracing::warn!("设置目录创建失败: {e}");
    }
    if let Err(e) = std::fs::write(
        &path,
        serde_json::to_string_pretty(settings).unwrap_or_else(|_| "{}".into()),
    ) {
        tracing::warn!("设置持久化失败: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("stross-settings-test-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("创建临时目录");
        d
    }

    #[test]
    fn default_is_discoverable_false() {
        assert!(!Settings::default().discoverable);
    }

    #[test]
    fn load_missing_returns_default() {
        let dir = tmp_dir("missing");
        let s = load_or_default(&dir);
        assert!(!s.discoverable);
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = tmp_dir("roundtrip");
        let s = Settings { discoverable: true };
        save(&dir, &s);
        assert_eq!(load_or_default(&dir), s);
    }

    #[test]
    fn load_bad_json_returns_default() {
        let dir = tmp_dir("bad");
        std::fs::write(dir.join(SETTINGS_FILE), "{oops").ok();
        assert_eq!(load_or_default(&dir), Settings::default());
    }
}
