//! 节点 / 端点 / 角色身份（`message/ids` 拆分：node.rs）。

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::media::MediaKind;

/// 端点 id（端点身份强类型：**kind 枚举 + 数值子 id**，替代裸字符串）。
///
/// 设计（user story：id 管理靠「约定 + 强类型 + 注册表」，人类可读内容
/// ——文件名/设备名——**不进 id**，走注册表 `name`/`FileSource` 查询）：
///
/// * `kind`：枚举（[`MediaKind`]，单一真源 `as_str`），**根治前缀字符串漂移**
///   （旧字符串 `"sysaudio:builtin"` vs `MediaKind::SystemAudio.as_str() ==
///   "systemAudio"` 不一致）；
/// * `id`：**数值子 id**，仅保证**本机族内唯一**（`screen` 的第 N 块屏）。
///   跨设备唯一性由命名空间 `(device_id, endpoint_id)` 组合保证——网格无
///   全局分配器，端点 id 是**局部句柄**；
/// * 线上序列化见各 wire 结构体：`endpoint_id` 独立数值字段 + `kind` 独立
///   枚举字段（方案 A，wire 无前缀冗余）。
///
/// 本类型是内核注册表键位 / [`super::super::endpoint::SubscribeSpec`] /
/// [`crate::contract::Endpoint::id`] 的**内部承载**（Copy + Hash + Eq），
/// 不强加 `String`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EndpointId {
    pub kind: MediaKind,
    /// 族内数值子 id（`kind` 内唯一；由注册表分配/约定）。
    pub id: u32,
}

impl EndpointId {
    /// 构造族内子 id（如 `EndpointId::of(MediaKind::Screen, 0)`）。
    pub const fn new(kind: MediaKind, id: u32) -> Self {
        Self { kind, id }
    }

    /// 从可读串 `"<kind>:<id>"` 解析（如 `"systemAudio:0"` / `"screen:2"`；
    /// kind 用 [`MediaKind::as_str`] 单一真源，丢弃旧 `"mic:builtin"` 魔法串）。
    /// 仅用于命令参数 / 日志 / 展示的**可读形态**；wire 不依赖本函数。
    pub fn parse(s: &str) -> Option<Self> {
        let (kind, id) = s.split_once(':')?;
        Some(Self {
            kind: MediaKind::from_wire(kind)?,
            id: id.parse().ok()?,
        })
    }

    /// 可读串形态（与 [`Self::parse`] 互逆）：`"{kind}:{id}"`。
    pub fn to_wire_str(&self) -> String {
        format!("{}:{}", self.kind.as_str(), self.id)
    }
}

impl std::fmt::Display for EndpointId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_wire_str())
    }
}

impl serde::Serialize for EndpointId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        // 控制面 / 日志 JSON 用可读串 `"kind:id"`（MediaKind::as_str 单一真源）；
        // 跨设备 wire（EndpointSummary/Manifest/SubscribeSpec）用独立
        // `endpoint_id: u32` + `kind` 字段，不经本实现。
        s.serialize_str(&self.to_wire_str())
    }
}

impl<'de> serde::Deserialize<'de> for EndpointId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::parse(&s).ok_or_else(|| serde::de::Error::custom(format!("非法端点 id: {s}")))
    }
}

/// 节点全局物理/拓扑标识（16 字节定长原语，Copy 语义，零堆分配）。
///
/// 在内存与二进制线序中直接为 `[u8; 16]`（2 个 CPU 寄存器大小，哈希/比对仅 1~2 条指令）；
/// 在 JSON / mDNS TXT / 日志中呈现为 32 位十六进制字符串（或由 `from_seed` 从短测试字符串确定性映射）。
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, ToSchema)]
#[schema(value_type = String, example = "0123456789abcdef0123456789abcdef")]
pub struct NodeId(pub [u8; 16]);

#[inline]
fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

impl NodeId {
    /// 全零空节点 ID。
    pub const NIL: Self = Self([0u8; 16]);

    /// 是否为空节点。
    pub const fn is_nil(&self) -> bool {
        let mut i = 0;
        while i < 16 {
            if self.0[i] != 0 {
                return false;
            }
            i += 1;
        }
        true
    }

    /// 是否为空节点（等价于 `is_nil`）。
    pub const fn is_empty(&self) -> bool {
        self.is_nil()
    }
    /// 由 16 字节原始数组构造。
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// 取底层 16 字节数组引用。
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// 消费自身，转为底层 16 字节数组。
    pub const fn into_bytes(self) -> [u8; 16] {
        self.0
    }

    /// 转为 32 位十六进制小写字符串。
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(32);
        for b in self.0 {
            use std::fmt::Write;
            let _ = write!(&mut s, "{b:02x}");
        }
        s
    }

    /// 从十六进制字符串解析（忽略可选的 `"node-"` 或 `"dev-"` 前缀）。
    pub fn from_hex(s: &str) -> Option<Self> {
        let hex = s
            .strip_prefix("node-")
            .or_else(|| s.strip_prefix("dev-"))
            .unwrap_or(s);
        if hex.len() != 32 {
            return None;
        }
        let mut bytes = [0u8; 16];
        for (i, chunk) in hex.as_bytes().as_chunks::<2>().0.iter().enumerate() {
            let hi = hex_nibble(chunk[0])?;
            let lo = hex_nibble(chunk[1])?;
            bytes[i] = (hi << 4) | lo;
        }
        Some(Self(bytes))
    }

    /// 从任意种子字符串确定性哈希派生 16 字节（测试 / 友好别名使用，如 `"alice"`, `"phone"`）。
    pub fn from_seed(seed: &str) -> Self {
        let mut h1 = 0xcbf2_9ce4_8422_2325u64;
        let mut h2 = 0x8422_2325_cbf2_9ce4u64;
        for b in seed.bytes() {
            h1 ^= u64::from(b);
            h1 = h1.wrapping_mul(0x0000_0100_0000_01b3);
            h2 ^= u64::from(b.rotate_left(3));
            h2 = h2.wrapping_mul(0x0000_0100_0000_01b3);
        }
        let mut bytes = [0u8; 16];
        bytes[0..8].copy_from_slice(&h1.to_le_bytes());
        bytes[8..16].copy_from_slice(&h2.to_le_bytes());
        Self(bytes)
    }

    /// 生成随机节点标识（读 `/dev/urandom`，不可用时回退时间戳 + 伪随机种子）。
    pub fn new_random() -> Self {
        let mut buf = [0u8; 16];
        let read_ok = std::fs::File::open("/dev/urandom")
            .and_then(|mut f| {
                use std::io::Read;
                f.read_exact(&mut buf)
            })
            .is_ok();
        if !read_ok {
            let t = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            buf[0..16].copy_from_slice(&t.to_le_bytes());
        }
        Self(buf)
    }
}

impl std::str::FromStr for NodeId {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(id) = Self::from_hex(s) {
            Ok(id)
        } else {
            Ok(Self::from_seed(s))
        }
    }
}

impl From<&str> for NodeId {
    fn from(s: &str) -> Self {
        s.parse().unwrap()
    }
}

impl From<String> for NodeId {
    fn from(s: String) -> Self {
        s.as_str().parse().unwrap()
    }
}

impl From<&String> for NodeId {
    fn from(s: &String) -> Self {
        s.as_str().parse().unwrap()
    }
}

impl From<[u8; 16]> for NodeId {
    fn from(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }
}

impl From<NodeId> for [u8; 16] {
    fn from(id: NodeId) -> Self {
        id.0
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

impl std::fmt::Debug for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "NodeId({})", self.to_hex())
    }
}

impl serde::Serialize for NodeId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            s.serialize_str(&self.to_hex())
        } else {
            self.0.serialize(s)
        }
    }
}

impl<'de> serde::Deserialize<'de> for NodeId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        if d.is_human_readable() {
            let s = String::deserialize(d)?;
            Ok(s.parse().unwrap())
        } else {
            let bytes = <[u8; 16]>::deserialize(d)?;
            Ok(Self(bytes))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_id_parse_roundtrip_and_kind_source_of_truth() {
        // 可读串 ↔ 强类型（命令参数/日志用）
        for (ep, s) in [
            (EndpointId::new(MediaKind::Screen, 0), "screen:0"),
            (EndpointId::new(MediaKind::SystemAudio, 2), "systemAudio:2"),
            (EndpointId::new(MediaKind::File, 77), "file:77"),
        ] {
            assert_eq!(ep.to_wire_str(), s);
            assert_eq!(EndpointId::parse(s), Some(ep));
        }
        assert_eq!(EndpointId::parse("nope:1"), None);
        assert_eq!(EndpointId::parse("screen:abc"), None); // 子 id 非数值
        assert_eq!(EndpointId::parse("screen:1:2"), None); // 多余冒号
        assert_eq!(EndpointId::parse("mic:builtin"), None); // 旧魔法串不再可解析

        // kind/id 强类型承载：比较与哈希按 (kind,id) 数值进行
        assert_eq!(
            EndpointId::new(MediaKind::Screen, 0),
            EndpointId::new(MediaKind::Screen, 0)
        );
        assert_ne!(
            EndpointId::new(MediaKind::Screen, 0),
            EndpointId::new(MediaKind::Mic, 0)
        );
        assert_ne!(
            EndpointId::new(MediaKind::Screen, 0),
            EndpointId::new(MediaKind::Screen, 1)
        );
    }
}
