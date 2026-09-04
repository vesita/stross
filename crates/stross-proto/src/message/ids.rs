//! 基础标识符枚举：传输 / 编解码 / 可靠性 / 能力 / 媒体 / 角色。
//!
//! 全部用枚举而非字符串，让编译器在匹配/比较时穷尽检查（代码规范）；
//! `rename_all` 保证线上 JSON 与 mDNS TXT 格式稳定。

use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
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
/// （docs/comm-mode-v2.md §3.0）。
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
/// 本类型是内核注册表键位 / [`super::endpoint::SubscribeSpec`] /
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

#[inline]
fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// 节点全局物理/拓扑标识（16 字节定长原语，Copy 语义，零堆分配）。
///
/// 在内存与二进制线序中直接为 `[u8; 16]`（2 个 CPU 寄存器大小，哈希/比对仅 1~2 条指令）；
/// 在 JSON / mDNS TXT / 日志中呈现为 32 位十六进制字符串（或由 `from_seed` 从短测试字符串确定性映射）。
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, ToSchema)]
#[schema(value_type = String, example = "0123456789abcdef0123456789abcdef")]
pub struct NodeId(pub [u8; 16]);

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

/// 数据面流标识符（栈内联小字符串，≤23 字节零堆分配，具备强类型隔离与高吞吐）。
///
/// 避免散落的 `String` 堆分配，同时防止将流 ID 与节点 ID / 链路 ID 混淆。
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default, ToSchema)]
#[schema(value_type = String, example = "screen-0-ly-rt")]
pub struct StreamId(pub SmolStr);

impl StreamId {
    pub fn new(s: impl AsRef<str>) -> Self {
        Self(SmolStr::new(s.as_ref()))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn into_string(self) -> String {
        self.0.to_string()
    }
}

impl std::ops::Deref for StreamId {
    type Target = str;
    fn deref(&self) -> &str {
        self.0.as_str()
    }
}

impl std::fmt::Display for StreamId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::str::FromStr for StreamId {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::new(s))
    }
}

impl From<&str> for StreamId {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl From<String> for StreamId {
    fn from(s: String) -> Self {
        Self(SmolStr::new(s))
    }
}

impl From<&String> for StreamId {
    fn from(s: &String) -> Self {
        Self::new(s)
    }
}

impl PartialEq<str> for StreamId {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for StreamId {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<String> for StreamId {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other.as_str()
    }
}

impl serde::Serialize for StreamId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for StreamId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(Self::new(s))
    }
}

/// 语义流全局标识（23 字节全局确定性构造，docs/comm-mode-v2.md §6）。
///
/// 一条流在拓扑中由 `(发布节点, 端点, 传输档案, pick规则)` 四要素数学唯一确定。
/// 双方握手后可在内存直接推导出完全一致的 23 字节结构体，零字符串拼接与清洗成本。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamKey {
    /// 发布方节点拓扑标识。
    pub publisher: NodeId,
    /// 资源端点标识。
    pub endpoint: EndpointId,
    /// 传输可靠性档案。
    pub profile: ReliabilityProfile,
    /// 管道装载/解读规则。
    pub pick: PickRule,
}

impl StreamKey {
    pub const fn new(
        publisher: NodeId,
        endpoint: EndpointId,
        profile: ReliabilityProfile,
        pick: PickRule,
    ) -> Self {
        Self {
            publisher,
            endpoint,
            profile,
            pick,
        }
    }

    /// 转换为可读的语义 StreamId。
    pub fn to_stream_id(&self) -> StreamId {
        derive_stream_id(&self.endpoint, self.profile, self.pick)
    }
}

impl std::fmt::Display for StreamKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.publisher, self.to_stream_id())
    }
}

/// 接收端链路标识（数值单调槽位，杜绝裸字符串与 "main" 魔法字符串）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, ToSchema)]
#[schema(value_type = String, example = "main")]
pub struct LinkId(pub u32);

impl LinkId {
    /// 预留单流/主链路兼容槽位。
    pub const MAIN: Self = Self(0);

    pub const fn new(id: u32) -> Self {
        Self(id)
    }

    pub const fn is_main(&self) -> bool {
        self.0 == 0
    }
}

impl std::fmt::Display for LinkId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_main() {
            write!(f, "main")
        } else {
            write!(f, "link-{}", self.0)
        }
    }
}

impl std::str::FromStr for LinkId {
    type Err = std::num::ParseIntError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "main" || s.is_empty() {
            return Ok(Self::MAIN);
        }
        if let Some(num) = s.strip_prefix("link-") {
            let n: u32 = num.parse()?;
            return Ok(Self(n));
        }
        let n: u32 = s.parse()?;
        Ok(Self(n))
    }
}

impl From<&str> for LinkId {
    fn from(s: &str) -> Self {
        s.parse().unwrap_or(Self::MAIN)
    }
}

impl From<u32> for LinkId {
    fn from(id: u32) -> Self {
        Self(id)
    }
}

impl serde::Serialize for LinkId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            s.serialize_str(&self.to_string())
        } else {
            self.0.serialize(s)
        }
    }
}

impl<'de> serde::Deserialize<'de> for LinkId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        if d.is_human_readable() {
            let s = String::deserialize(d)?;
            Ok(s.parse().unwrap_or(Self::MAIN))
        } else {
            let id = u32::deserialize(d)?;
            Ok(Self(id))
        }
    }
}

#[cfg(test)]
mod tests {
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
