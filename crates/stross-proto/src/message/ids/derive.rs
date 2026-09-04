//! 语义流 id 派生（`message/ids` 拆分：derive.rs）。

use super::node::EndpointId;
use super::stream::StreamId;
use super::transport::{PickRule, ReliabilityProfile};

/// 语义流 id 派生（docs/framework-v3.md §6「语义 id / 身份层」）：
/// `[端点 协议 解析]` 三要素确定性派生——**一个端点有且仅有一个 id**。
///
/// * **结构性订阅收敛**：同端点必然同 id，订阅方拿到目录 + 协商档案即可
///   本地推导（无需运行期查表、不依赖 grant 返回）；多订阅者 = 同一条流
///   （中继多 watcher 天然复用）；停一路只停该 id 数据面活动，互不级联；
/// * **id 可推导 ≠ 可接入**：受控中继仍校验 Hello 接入（本机回环预授权 /
///   跨设备一次性凭证 [`super::super::token::ShareToken`]）；
/// * codec 为可扩展维度：同端点同刻只产一种编码，暂不进 id 三要素；未来
///   若同端点多 codec 多路，扩为 `[端点 协议 解析 codec]` 即可。
///
/// 编码：可读前缀（URL 安全的端点 id 字符串 + 档案短名）+ FNV-1a 哈希尾
/// （确定性、长度受控、无随机 seed 依赖）。端点 id 以 [`EndpointId::to_wire_str`]
/// 形态（`"kind:id"`，kind 用 [`MediaKind::as_str`] 单一真源）进入——不再有
/// 手工拼接的魔法前缀，也根治 `sysaudio`/`systemAudio` 之类的漂移。
pub fn derive_stream_id(
    endpoint_id: &EndpointId,
    profile: ReliabilityProfile,
    pick: PickRule,
) -> StreamId {
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
    // 端点 id 字符串（`kind:id`，全为 URL 安全字符：kind 小写 + 数字冒号）→
    // 清洗（防极端 kind 演进引入非安全字符）+ 档案短名前缀。
    let raw = endpoint_id.to_wire_str();
    let mut cleaned: Vec<u8> = Vec::with_capacity(raw.len());
    for b in raw.bytes() {
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
    StreamId::new(format!("{prefix}-{:08x}", h & 0xffff_ffff))
}

#[cfg(test)]
mod tests {
    use super::super::media::MediaKind;
    use super::*;

    #[test]
    fn derive_stream_id_is_deterministic_and_distinct() {
        // 同三要素 → 同 id（结构性订阅收敛的核心保证）
        let screen0 = EndpointId::new(MediaKind::Screen, 0);
        let a = derive_stream_id(&screen0, ReliabilityProfile::Lossy, PickRule::Realtime);
        let b = derive_stream_id(&screen0, ReliabilityProfile::Lossy, PickRule::Realtime);
        assert_eq!(a, b, "同端点同档案必须派生同 id");
        // 档案任一项不同 → 不同 id（协议 / 解析是 id 三要素）
        assert_ne!(
            a,
            derive_stream_id(&screen0, ReliabilityProfile::Lossless, PickRule::Realtime),
            "协议不同必须派生不同 id"
        );
        assert_ne!(
            a,
            derive_stream_id(&screen0, ReliabilityProfile::Lossy, PickRule::StrictOrdered),
            "解析规则不同必须派生不同 id"
        );
        // 端点不同（kind 或 子 id 不同）→ 不同 id
        assert_ne!(
            a,
            derive_stream_id(
                &EndpointId::new(MediaKind::Mic, 0),
                ReliabilityProfile::Lossy,
                PickRule::Realtime
            ),
            "端点 kind 不同必须派生不同 id"
        );
        assert_ne!(
            a,
            derive_stream_id(
                &EndpointId::new(MediaKind::Screen, 1),
                ReliabilityProfile::Lossy,
                PickRule::Realtime
            ),
            "端点子 id 不同必须派生不同 id"
        );
    }

    #[test]
    fn derive_stream_id_is_url_safe_and_bounded() {
        for (ep, profile, pick) in [
            (
                EndpointId::new(MediaKind::Screen, 0),
                ReliabilityProfile::Lossy,
                PickRule::Realtime,
            ),
            (
                EndpointId::new(MediaKind::File, 12),
                ReliabilityProfile::Lossless,
                PickRule::StrictOrdered,
            ),
            (
                EndpointId::new(MediaKind::Mic, 0),
                ReliabilityProfile::Lossy,
                PickRule::Realtime,
            ),
            (
                EndpointId::new(MediaKind::SystemAudio, 0),
                ReliabilityProfile::Adaptive,
                PickRule::None,
            ),
            (
                EndpointId::new(MediaKind::Screen, 1),
                ReliabilityProfile::Lossy,
                PickRule::Realtime,
            ),
        ] {
            let id = derive_stream_id(&ep, profile, pick);
            assert!(
                id.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_'),
                "派生 id 必须 URL 安全: {id}"
            );
            assert!(id.len() <= 80, "派生 id 长度受控: {id}");
            assert!(id.contains(profile_short(profile)), "前缀含协议短名: {id}");
            assert!(id.contains(pick_short(pick)), "前缀含解析短名: {id}");
        }
        // 前缀可读：端点 id（`kind:id`）清洗后出现在派生 id 里，用 MediaKind 单一真源
        let id = derive_stream_id(
            &EndpointId::new(MediaKind::Screen, 0),
            ReliabilityProfile::Lossy,
            PickRule::Realtime,
        );
        assert!(id.starts_with("screen-0-ly-rt-"), "可读前缀: {id}");
        // 系统性根治旧魔法串：SystemAudio kind 用 `systemAudio`（不再是错的 sysaudio）
        let id = derive_stream_id(
            &EndpointId::new(MediaKind::SystemAudio, 0),
            ReliabilityProfile::Lossy,
            PickRule::Realtime,
        );
        assert!(
            id.starts_with("systemAudio-0-ly-rt-"),
            "SystemAudio kind 前缀漂移已根治: {id}"
        );
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
