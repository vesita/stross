//! # stross-proto
//!
//! 定义 Stross 的线上协议：
//!
//! * **媒体帧**（二进制 WebSocket 消息）：固定 24 字节头（v2，含帧序号与分片字段）+ 载荷。
//! * **控制消息**（文本 WebSocket 消息）：JSON，见 [`message`](crate::message)，
//!   含能力协商（`Capabilities`/`Offer`/`Answer`）与路由控制（`Route`）。

pub mod frame;
pub mod message;

/// 通用时间工具（Unix 秒/毫秒；多处会话起点/过期时间计算共用，避免各层
/// 重复 `SystemTime::duration_since(UNIX_EPOCH)` 转换）。
pub mod time {
    /// 当前 Unix 秒。
    pub fn unix_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// 当前 Unix 毫秒。
    pub fn unix_millis() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

pub use frame::*;
pub use message::*;
