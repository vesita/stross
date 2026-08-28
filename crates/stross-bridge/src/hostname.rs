//! 本机主机名（平台适应：OS 调用收敛在桥接层）。
//!
//! 内核红线：零 OS 调用。mDNS 广播名 / 默认设备名等需要主机名的地方，
//! 一律由壳层经这里取值后**注入**内核（`Discovery::start`、`ensure_identity`
//! 等都接收 hostname 参数）。

/// 本机主机名；失败时回退 `fallback`（调用方按用途给默认值）。
pub fn hostname_or(fallback: &str) -> String {
    hostname::get()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_else(|_| fallback.into())
}

/// 占位主机名（无标识意义）：空 / `localhost` / `android`。
fn is_placeholder_hostname(h: &str) -> bool {
    h.is_empty() || h == "localhost" || h == "android"
}

/// 本机**设备名**（mDNS 广播名 / 默认设备标识用）。
///
/// 与 [`hostname_or`] 的区别：Android 平台主机名恒为 `localhost`
/// （`/proc/sys/kernel/hostname`），直接广播会产生「localhost」这种
/// 无标识意义的名字；这里把空值 / `localhost` / `android` 过滤掉，
/// 回退调用方给的品牌名（如「Stross 设备」），其余平台返回真实主机名
/// （对端看到的设备标识 = 设备自身主机名，消除「本机中继」式歧义）。
pub fn device_name_or(fallback: &str) -> String {
    let h = hostname::get()
        .map(|h| h.to_string_lossy().trim().to_string())
        .unwrap_or_default();
    if is_placeholder_hostname(&h) {
        fallback.to_string()
    } else {
        h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_never_panics() {
        let _ = hostname_or("stross");
        let _ = device_name_or("Stross 设备");
    }

    #[test]
    fn placeholder_hostname_detection() {
        assert!(is_placeholder_hostname(""));
        assert!(is_placeholder_hostname("localhost"));
        assert!(is_placeholder_hostname("android"));
        assert!(!is_placeholder_hostname("noxy"));
        assert!(!is_placeholder_hostname("Stross-PC"));
    }

    #[test]
    fn device_name_returns_real_hostname_when_meaningful() {
        let h = hostname::get()
            .map(|h| h.to_string_lossy().trim().to_string())
            .unwrap_or_default();
        if is_placeholder_hostname(&h) {
            assert_eq!(device_name_or("Stross 设备"), "Stross 设备");
        } else {
            assert_eq!(device_name_or("Stross 设备"), h);
        }
    }
}
