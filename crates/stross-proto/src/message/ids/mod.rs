//! 基础标识符枚举：传输 / 编解码 / 可靠性 / 能力 / 媒体 / 角色。
//!
//! 全部用枚举而非字符串，让编译器在匹配/比较时穷尽检查（代码规范）；
//! `rename_all` 保证线上 JSON 与 mDNS TXT 格式稳定。
//!
//! 模块拆分（v3 §7）：`message/ids/` 目录——wire 类型真源在此，`message` 根
//! 经本模块统一重导出（`stross_proto::message::*` 路径不变）：
//! * [`transport`]：传输 / 可靠性 / pick / 能力枚举（[`TransportId`] /
//!   [`ReliabilityProfile`] / [`PickRule`] / [`CapabilityKind`]）；
//! * [`media`]：媒体 / 编解码（[`MediaKind`] / [`CodecId`]）；
//! * [`node`]：节点 / 端点 / 角色身份（[`NodeId`] / [`EndpointId`] / [`RoleId`]）；
//! * [`stream`]：流 / 链路标识（[`StreamId`] / [`StreamKey`] / [`LinkId`] /
//!   [`StreamRole`]）；
//! * [`derive`]：语义流 id 派生（[`derive_stream_id`]）。

mod derive;
mod media;
mod node;
mod stream;
mod transport;

pub use derive::derive_stream_id;
pub use media::{CodecId, MediaKind};
pub use node::{EndpointId, EndpointRef, NodeId, RoleId};
pub use stream::{LinkId, StreamId, StreamKey, StreamRole};
pub use transport::{CapabilityKind, PickRule, ReliabilityProfile, TransportId};
