//! 传输 / 可靠性 / pick / 能力枚举（`message/ids` 拆分：transport.rs）。

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// 传输标识（有限集合）。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum TransportId {
    /// WebSocket（TCP，无损）。
    Ws,
    /// WebRTC data channel（UDP，有损低延迟）。
    WebRtc,
    /// SRT（ARQ + 时延预算，自适应）。
    Srt,
    /// QUIC（多路复用，无损）。
    Quic,
    /// 内存传输（测试 / 示例用）。
    Memory,
}

/// 传输可靠性契约（设计文档 §4.1）。
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    Default,
    ToSchema,
)]
#[serde(rename_all = "camelCase")]
pub enum ReliabilityProfile {
    /// TCP-like：控制消息、输入注入、剪贴板 —— 全序不丢。
    Lossless,
    /// UDP-like：媒体帧 —— 允许丢帧，靠关键帧对齐自愈。
    #[default]
    Lossy,
    /// SRT-like：ARQ + 时延预算，超时则丢。
    Adaptive,
}

/// pick 规则（pick rule）：数据管道「装载/解读」的语义规则
/// （docs/framework-v3.md §3.0）。
///
/// 与 [`ReliabilityProfile`]（传输层「怎么送」）正交：本枚举描述数据面
/// 「怎么处理」——发送侧装载逻辑与接收侧解读逻辑共用同一对 pick 规则，
/// 协商定稿后内核按 id 装载对应模块。
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    Default,
    ToSchema,
)]
#[serde(rename_all = "camelCase")]
pub enum PickRule {
    /// 严格即时（Realtime）：低延迟、按 PTS 调度、容忍丢帧丢块
    /// （关键帧对齐自愈）。视频/音频实时目标默认。
    #[default]
    Realtime,
    /// 严格顺序（StrictOrdered）：严格有序、重传、逐字节不丢。
    /// 文件/剪贴板确定目标默认。
    StrictOrdered,
    /// 无处理语义（纯直通；不装载处理模块）。
    None,
}

/// 能力种类：采集（Source）或 接收/注入（Sink）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CapabilityKind {
    Source,
    Sink,
}
