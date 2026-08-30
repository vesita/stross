//! 基础标识符枚举：传输 / 编解码 / 可靠性 / 能力 / 媒体 / 角色。
//!
//! 全部用枚举而非字符串，让编译器在匹配/比较时穷尽检查（代码规范）；
//! `rename_all` 保证线上 JSON 与 mDNS TXT 格式稳定。

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

/// 编解码标识（有限集合，可扩展）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum CodecId {
    H264,
    Aac,
    Opus,
    /// AV1（预留；传输/编码器支持后启用）。
    Av1,
}

/// 传输可靠性契约（设计文档 §4.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
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

/// 能力种类：采集（Source）或 接收/注入（Sink）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CapabilityKind {
    Source,
    Sink,
}

/// 媒体能力类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum MediaKind {
    Screen,
    Window,
    Camera,
    Mic,
    SystemAudio,
    Input,
    Clipboard,
    /// 文件互传（二期 E：ReliableChannel；Lossless 传输）。
    File,
    /// 程序服务端点（占位：schema 后置，暂不可订阅）。
    Service,
}

impl MediaKind {
    /// wire 字符串（camelCase；与 serde 序列化一致，单一真源）。
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Screen => "screen",
            Self::Window => "window",
            Self::Camera => "camera",
            Self::Mic => "mic",
            Self::SystemAudio => "systemAudio",
            Self::Input => "input",
            Self::Clipboard => "clipboard",
            Self::File => "file",
            Self::Service => "service",
        }
    }
}

/// 设备角色（发现广播 F1.2 用；有限集合）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RoleId {
    /// 可作源（推流）。
    Sender,
    /// 可作汇（接收播放）。
    Viewer,
    /// 中继（转发数据面）。
    Relay,
    /// 控制者（控制面；D7 远程控制阶段开放）。
    Controller,
}
