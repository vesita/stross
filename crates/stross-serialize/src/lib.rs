//! **序列化协议**：原始数据 ↔ 线格式（装载 / 解装载）。
//!
//! v3 概念定稿（docs/framework-v3.md §3.6）：序列化 = 数据管道「原始数据 →
//! 线格式」的转化，**端点自决载荷打包**（编码/压缩/元数据/分块粒度），
//! 内核提供统一信封（[`Frame`]：id+len+顺序字段）保证中继可转发。
//!
//! * [`SerializeRule`] 枚举（wire 可比对，定义在 stross-proto）标识规则；
//! * [`Loader`]（装载，发送侧）：原始数据 → 帧（打包/分包）；
//! * [`Unloader`]（解装载，接收侧）：帧 → 原始数据（重组/校验）。
//!
//! 当前唯一实现 `Passthrough`（直通）；`Chunked`（分包）为预留规则——
//! 装载工厂对未实现的规则返回明确拒绝（不静默降级）。

use stross_proto::frame::Frame;
use stross_proto::message::TrackInfo;

mod passthrough;

pub use passthrough::{PassthroughLoader, loader_for};
pub use stross_proto::message::SerializeRule;

/// 装载逻辑（发送侧）：把「原始数据」装载为可发送的线上帧。
///
/// 与 [`Unloader`]（解装载逻辑）两端对称，共用同一 [`SerializeRule`]。
/// 端点自决载荷打包（编码/压缩/元数据/分块粒度），本 trait 定义装载边界。
pub trait Loader: Send {
    /// 本装载器实现的序列化规则（数据契约；`Passthrough` 当前唯一实现）。
    fn serialize_rule(&self) -> SerializeRule;
    /// 装载一个数据包为线上帧（当前直通；分包规则在装载器内实现）。
    fn load(&self, track: TrackInfo, data: &[u8], pts_ms: u32) -> Vec<Frame>;
}

/// 解装载逻辑（接收侧）：把线上帧还原为「原始数据」（重组/校验）。
///
/// 与 [`Loader`] 对称；`Unloader` 是有状态对象（跨帧重组需要缓冲）。
pub trait Unloader: Send {
    /// 本解装载器实现的序列化规则。
    fn serialize_rule(&self) -> SerializeRule;
    /// 解装载一帧；返回完整的原始数据包（`None` = 尚未重组完成）。
    fn unpack(&mut self, frame: Frame) -> Option<Vec<u8>>;
}
