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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_never_panics() {
        let _ = hostname_or("stross");
    }
}
