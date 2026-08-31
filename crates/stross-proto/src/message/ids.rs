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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, ToSchema)]
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
/// （docs/comm-mode-v2.md §3.0）。
///
/// 与 [`ReliabilityProfile`]（传输层「怎么送」）正交：本枚举描述数据面
/// 「怎么处理」——发送侧装载逻辑与接收侧解读逻辑共用同一对 pick 规则，
/// 协商定稿后内核按 id 装载对应模块。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, ToSchema)]
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

/// 语义流 id 派生（docs/comm-mode-v2.md §6「语义 id / 身份层」）：
/// `[端点 协议 解析]` 三要素确定性派生——**一个端点有且仅有一个 id**。
///
/// * **结构性订阅收敛**：同端点必然同 id，订阅方拿到目录 + 协商档案即可
///   本地推导（无需运行期查表、不依赖 grant 返回）；多订阅者 = 同一条流
///   （中继多 watcher 天然复用）；停一路只停该 id 数据面活动，互不级联；
/// * **id 可推导 ≠ 可接入**：受控中继仍校验 Hello 接入（本机回环预授权 /
///   跨设备一次性凭证 [`super::token::ShareToken`]）；
/// * codec 为可扩展维度：同端点同刻只产一种编码，暂不进 id 三要素；未来
///   若同端点多 codec 多路，扩为 `[端点 协议 解析 codec]` 即可。
///
/// 编码：可读前缀（URL 安全的清洗端点 id + 档案短名）+ FNV-1a 哈希尾
/// （确定性、长度受控、无随机 seed 依赖）。
pub fn derive_stream_id(endpoint_id: &str, profile: ReliabilityProfile, pick: PickRule) -> String {
    let profile_short = match profile {
        ReliabilityProfile::Lossless => "ll",
        ReliabilityProfile::Lossy => "ly",
        ReliabilityProfile::Adaptive => "ad",
    };
    let pick_short = match pick {
        PickRule::Realtime => "rt",
        PickRule::StrictOrdered => "so",
        PickRule::None => "n",
    };
    // 前缀 = 清洗端点 id（保留 [A-Za-z0-9._-]，其余压成 `-`，长度封顶）+
    // 档案短名——日志/UI 可读，且三要素已全部进入前缀（哈希只控长）。
    let mut cleaned: Vec<u8> = Vec::with_capacity(endpoint_id.len());
    for b in endpoint_id.bytes() {
        let c = match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-' => b,
            _ => b'-',
        };
        if cleaned.last() != Some(&c) || c != b'-' {
            cleaned.push(c); // 折叠连续 `-`
        }
    }
    while cleaned.last() == Some(&b'-') {
        cleaned.pop();
    }
    if cleaned.is_empty() {
        cleaned.push(b'e');
    }
    cleaned.truncate(48);
    let prefix = format!(
        "{}-{profile_short}-{pick_short}",
        String::from_utf8_lossy(&cleaned)
    );
    // FNV-1a 64：确定性哈希（不引入随机 seed），取低 32 位 hex。
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in prefix.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{prefix}-{:08x}", h & 0xffff_ffff)
}

/// 流在共享连接上的方向（通信模式 v2 Phase C「连接复用」：
/// [`super::control::ControlMessage::OpenStream`] 用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamRole {
    /// 推流（等价旧 Hello：推流端声明开始推流）。
    Push,
    /// 观看（等价旧 Watch：观看端请求观看一个流）。
    Watch,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_stream_id_is_deterministic_and_distinct() {
        // 同三要素 → 同 id（结构性订阅收敛的核心保证）
        let a = derive_stream_id("screen:0", ReliabilityProfile::Lossy, PickRule::Realtime);
        let b = derive_stream_id("screen:0", ReliabilityProfile::Lossy, PickRule::Realtime);
        assert_eq!(a, b, "同端点同档案必须派生同 id");
        // 档案任一项不同 → 不同 id（协议 / 解析是 id 三要素）
        assert_ne!(
            a,
            derive_stream_id("screen:0", ReliabilityProfile::Lossless, PickRule::Realtime),
            "协议不同必须派生不同 id"
        );
        assert_ne!(
            a,
            derive_stream_id(
                "screen:0",
                ReliabilityProfile::Lossy,
                PickRule::StrictOrdered
            ),
            "解析规则不同必须派生不同 id"
        );
        // 端点不同 → 不同 id
        assert_ne!(
            a,
            derive_stream_id("mic:builtin", ReliabilityProfile::Lossy, PickRule::Realtime),
            "端点不同必须派生不同 id"
        );
    }

    #[test]
    fn derive_stream_id_is_url_safe_and_bounded() {
        for (ep, profile, pick) in [
            ("screen:0", ReliabilityProfile::Lossy, PickRule::Realtime),
            (
                "file:备注.txt",
                ReliabilityProfile::Lossless,
                PickRule::StrictOrdered,
            ),
            ("mic:builtin", ReliabilityProfile::Lossy, PickRule::Realtime),
            (
                "a/b\\c d e::f",
                ReliabilityProfile::Adaptive,
                PickRule::None,
            ),
            ("系统声音", ReliabilityProfile::Lossy, PickRule::Realtime),
        ] {
            let id = derive_stream_id(ep, profile, pick);
            assert!(
                id.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_'),
                "派生 id 必须 URL 安全: {id}"
            );
            assert!(id.len() <= 80, "派生 id 长度受控: {id}");
            assert!(id.contains(profile_short(profile)), "前缀含协议短名: {id}");
            assert!(id.contains(pick_short(pick)), "前缀含解析短名: {id}");
        }
        // 前缀可读：端点 id 清洗后出现在派生 id 里
        let id = derive_stream_id("screen:0", ReliabilityProfile::Lossy, PickRule::Realtime);
        assert!(id.starts_with("screen-0-ly-rt-"), "可读前缀: {id}");
    }

    fn profile_short(p: ReliabilityProfile) -> &'static str {
        match p {
            ReliabilityProfile::Lossless => "ll",
            ReliabilityProfile::Lossy => "ly",
            ReliabilityProfile::Adaptive => "ad",
        }
    }

    fn pick_short(p: PickRule) -> &'static str {
        match p {
            PickRule::Realtime => "rt",
            PickRule::StrictOrdered => "so",
            PickRule::None => "n",
        }
    }
}
