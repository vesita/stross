//! 本机主机名（平台适应：OS 调用收敛在桥接层）。
//!
//! 内核红线：零 OS 调用。mDNS 广播名 / 默认设备名等需要主机名的地方，
//! 一律由壳层经这里取值后**注入**内核（`Discovery::start`、`ensure_identity`
//! 等都接收 hostname 参数）。

/// 本机主机名；失败时回退 `fallback`（调用方按用途给默认值）。
pub fn hostname_or(fallback: &str) -> String {
    hostname::get().map_or_else(|_| fallback.into(), |h| h.to_string_lossy().to_string())
}

/// 占位主机名判定（空 / `localhost` / `android`）：**单一真源在 stross-types**
/// （桥接层与内核共用——内核不依赖本层，故上移到双方依赖的契约层）。
pub use stross_types::hostname::is_placeholder as is_placeholder_hostname;

#[cfg(target_os = "android")]
fn android_device_model() -> Option<String> {
    let out = std::process::Command::new("getprop")
        .arg("ro.product.model")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let model = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if model.is_empty() {
        return None;
    }
    let brand = std::process::Command::new("getprop")
        .arg("ro.product.brand")
        .output()
        .ok()
        .and_then(|b| {
            if b.status.success() {
                let s = String::from_utf8_lossy(&b.stdout).trim().to_string();
                if !s.is_empty() { Some(s) } else { None }
            } else {
                None
            }
        });
    match brand {
        Some(b) if !model.to_lowercase().contains(&b.to_lowercase()) => {
            Some(format!("{b} {model}"))
        }
        _ => Some(model),
    }
}

/// 本机**设备名**（mDNS 广播名 / 默认设备标识用）。
///
/// 与 [`hostname_or`] 的区别：Android 平台主机名恒为 `localhost`
/// （`/proc/sys/kernel/hostname`），直接广播会产生「localhost」这种
/// 无标识意义的名字；这里把空值 / `localhost` / `android` 过滤掉，
/// 优先在 Android 读取系统品牌与型号（如「OnePlus PLC110」），其余平台返回真实主机名，
/// 均无可用标识时回退调用方给的品牌名（如「Stross 设备」）。
pub fn device_name_or(fallback: &str) -> String {
    let h = hostname::get()
        .map(|h| h.to_string_lossy().trim().to_string())
        .unwrap_or_default();
    if is_placeholder_hostname(&h) {
        #[cfg(target_os = "android")]
        if let Some(model) = android_device_model() {
            return model;
        }
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
