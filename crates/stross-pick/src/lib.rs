//! **pick 规则**：数据管道「装载/解读」的语义规则。
//!
//! v3 概念定稿（docs/framework-v3.md §3.7）：**原始数据 → 装载逻辑 → 传输数据
//! → 接收数据 → 解读逻辑 → 呈现数据**。pick 规则（[`PickRule`]，定义在
//! stross-proto，wire 可比对）是装载/解读的语义规则，目前两种：
//! **严格顺序（StrictOrdered）** / **严格即时（Realtime）**。
//!
//! * [`Interpreter`]（解读，接收侧）：按规则把帧流还原为呈现数据
//!   （[`RealtimePacing`] 低延迟容忍丢帧 / [`StrictOrdered`] 逐字节不丢；
//!   有损路径经 [`JitterBuffer`] 按序/按关键帧对齐产出）；
//! * [`Pacing`]（装载调度，发送侧）：按规则打发送节奏（契约；实现随发送侧
//!   落地）。
//!
//! 两端对称：装载与解读是同一对 pick 规则的两端对称实现，均由内核提供
//! 通用框架；端点只实现「原始数据 ↔ 传输数据」的转化，经内核 trait 调用。
//! 本 crate 依赖方向只有 stross-proto——`ChannelKind`（无损/有损分流）由
//! 调用方按传输可靠性契约计算后传入（transport 判断留在 kernel）。

mod buffer;
mod interpret;
mod registry;

use stross_proto::frame::Frame;

pub use buffer::{JitterBuffer, JitterConfig, JitterStats};
pub use interpret::{RealtimePacing, StrictOrdered};
pub use registry::{ChannelKind, InterpretRegistry, StreamChannel};
pub use stross_proto::message::PickRule;

/// 帧消费出口（解读结果交付：呈现 / 播放 / 落盘）。
pub trait FrameSink: Send {
    fn push(&mut self, frame: Frame);
}

/// 解读逻辑（接收侧，per-stream 实例）：按 pick 规则把帧流还原。
///
/// * `RealtimePacing`：低延迟、按 PTS 调度、容忍丢帧丢块（关键帧对齐自愈）；
/// * `StrictOrdered`：严格有序、防御式丢弃乱序/重复（逐字节不丢）。
pub trait Interpreter: Send {
    /// 本解读器的 pick 规则。
    fn rule(&self) -> PickRule;
    /// 推入一帧（有损路径先落抖动缓冲；无损路径直通）。
    fn push(&mut self, frame: Frame);
    /// 取出一帧解读结果（`None` = 暂无就绪帧）。
    fn poll(&mut self) -> Option<Frame>;
}

/// 装载调度（发送侧）：按 pick 规则打发送节奏。
///
/// * `StrictOrdered`：按序完整发送（不丢不乱）；
/// * `Realtime`：即时直通（容忍丢帧丢块）。
pub trait Pacing: Send {
    /// 本装载调度的 pick 规则。
    fn rule(&self) -> PickRule;
    /// 调度一帧（按规则节流 / 直通）。
    fn emit(&self, frame: Frame, sink: &mut dyn FrameSink);
}
