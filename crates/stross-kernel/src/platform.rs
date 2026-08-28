//! 运行平台标签（**纯值**，无任何平台调用 / 分支）。
//!
//! 平台知识（哪个平台有哪些设备能力、数据目录在哪）在 [`stross_bridge`]：
//! 壳层经桥接层判定平台并注入内核；内核只保存这个标签用于展示
//! （`app_info().platform`）与设备清单种子的选择依据。

/// 运行平台（UI 层经 [`stross_bridge::devices::platform`] 判定后注入）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Desktop,
    Android,
}

impl Platform {
    pub fn as_str(&self) -> &'static str {
        match self {
            Platform::Desktop => "desktop",
            Platform::Android => "android",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(Platform::Desktop.as_str(), "desktop");
        assert_eq!(Platform::Android.as_str(), "android");
    }
}
