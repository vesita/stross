//! 流 / 链路标识（`message/ids` 拆分：stream.rs）。

use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use utoipa::ToSchema;

use super::derive::derive_stream_id;
use super::node::{EndpointId, NodeId};
use super::transport::{PickRule, ReliabilityProfile};

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

/// 语义流全局标识（23 字节全局确定性构造，docs/framework-v3.md §6）。
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

/// 流在共享连接上的方向（通信模式 v2 Phase C「连接复用」：
/// [`super::super::control::ControlMessage::OpenStream`] 用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamRole {
    /// 推流（等价旧 Hello：推流端声明开始推流）。
    Push,
    /// 观看（等价旧 Watch：观看端请求观看一个流）。
    Watch,
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
