//! 平台无关的主机名/设备名判定（跨层共享纯逻辑）。
//!
//! 分层红线（docs/framework-v3.md）：内核（stross-kernel）不依赖
//! 平台桥接层（stross-bridge），而两端都需要「主机名是否无标识意义」的判定
//! （桥接层：`device_name_or` 回退；内核：身份注入时覆盖占位名）。故该纯逻辑
//! 上移到双方都依赖的契约层 stross-view，单一真源，消灭分层被迫的重复内联。

/// 主机名/设备名是否无标识意义（空 / `localhost` / `android`）。
///
/// Android 平台 `/proc/sys/kernel/hostname` 恒为 `localhost`，直接广播会产生
/// 「localhost」这种无意义节点名；判定后应回退调用方给的品牌名（如「Stross 设备」）。
pub fn is_placeholder(name: &str) -> bool {
    let n = name.trim();
    n.is_empty() || n == "localhost" || n == "android"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_detection() {
        assert!(is_placeholder(""));
        assert!(is_placeholder("  "));
        assert!(is_placeholder("localhost"));
        assert!(is_placeholder("android"));
        // 内部 trim：带空白的主机名同样视为占位
        assert!(is_placeholder(" localhost "));
        assert!(!is_placeholder("noxy"));
        assert!(!is_placeholder("Stross-PC"));
    }
}
