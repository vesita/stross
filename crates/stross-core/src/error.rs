//! 核心层结构化错误（替代散落的 `Result<_, String>`；调用方按语义匹配）。
//!
//! 应用层（[`stross_app::Error`]）在边界把本层错误转换为自己的语义变体，
//! 不再靠字符串拼接透传。

/// 观看连接错误（[`crate::watch::connect_watch`]）。
#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    /// 地址无法解析（未知 scheme / 缺端口）。
    #[error("无法解析中继地址: {0}")]
    InvalidUrl(String),
    /// 传输层拨号失败。
    #[error("连接中继失败: {0}")]
    Connect(String),
    /// 发送 Watch 请求失败（SRT/QUIC 带内声明）。
    #[error("发送 Watch 请求失败: {0}")]
    SendWatch(String),
    /// 中继拒绝观看（返回 Error 控制消息）。
    #[error("中继拒绝: {0}")]
    Rejected(String),
    /// 等待 Ready 回执失败 / 异常。
    #[error("等待中继就绪失败: {0}")]
    WaitReady(String),
    /// 中继在就绪前关闭连接。
    #[error("中继连接已关闭")]
    Closed,
}

/// 中继数据面操作错误（[`crate::relay::RelayState::start_proxy`]）。
#[derive(Debug, thiserror::Error)]
pub enum RelayOpError {
    /// 本地已有同名代理流。
    #[error("本地已有代理流 {0}")]
    ProxyExists(String),
    /// 本地已有同名流（推流或代理）。
    #[error("本地已有流 {0}（推流或代理）")]
    StreamExists(String),
}
