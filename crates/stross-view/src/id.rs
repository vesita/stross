//! 强类型 ID 公共路径（**定义单一真源留在 stross-proto**：线协议消息引用
//! 它们，迁入本 crate 会成环；本模块重导出为新公共路径）。
//!
//! 强类型标识符铁律（AGENTS.md）：实体标识符（节点、流、链路、策略、传输、
//! 消息等）与字典键严禁使用裸 `String` / `&str`，一律使用本模块的强类型。

pub use stross_proto::message::{
    CapabilityKind, CodecId, EndpointId, LinkId, MediaKind, MsgId, NodeId, PickRule,
    ReliabilityProfile, RoleId, SerializeRule, StrategyId, StreamId, StreamKey, StreamRole,
    TransferId, TransportId, derive_stream_id,
};
