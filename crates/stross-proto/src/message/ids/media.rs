//! 媒体 / 编解码枚举（`message/ids` 拆分：media.rs）。

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

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

/// 媒体能力类型。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, ToSchema,
)]
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
    // as_str / from_wire 由 define_wire_strings! 从下方 wire 表生成（单一真源）；
    // from_wire 供从 `"<kind>:<id>"` 可读串解析 `EndpointId` 用（见 super::mod）。
    crate::message::define_wire_strings! {
        MediaKind:
            Screen => "screen",
            Window => "window",
            Camera => "camera",
            Mic => "mic",
            SystemAudio => "systemAudio",
            Input => "input",
            Clipboard => "clipboard",
            File => "file",
            Service => "service",
    }
}

#[cfg(test)]
mod wire_consistency {
    use super::*;
    crate::message::assert_wire_strings_consistent! {
        mediatype_wire_matches_serde: MediaKind;
        MediaKind::Screen => "screen",
            MediaKind::Window => "window",
            MediaKind::Camera => "camera",
            MediaKind::Mic => "mic",
            MediaKind::SystemAudio => "systemAudio",
            MediaKind::Input => "input",
            MediaKind::Clipboard => "clipboard",
            MediaKind::File => "file",
            MediaKind::Service => "service",
    }
}
