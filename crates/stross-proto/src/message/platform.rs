//! 运行平台标签（**纯值**，无任何平台调用 / 分支）。
//!
//! 平台知识（哪个平台有哪些设备能力、数据目录在哪）在 [`stross_bridge`]：
//! 壳层经桥接层判定平台并注入；端点层（端点装配）与内核（展示 / 种子
//! 选择）共用这个标签。

/// 运行平台（UI 层经桥接层判定后注入）。
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
