//! 强类型 ID 单一真源：**严禁直接用裸 `String` / `&str` 作 key 或 id**。
//!
//! 全仓统一使用强类型新类型 / 枚举承载：
//! * [`NodeId`]：节点拓扑标识（16 字节定长原语，Copy 语义，零堆分配，Hex/Serde 兼容）
//! * [`EndpointId`]：端点身份（MediaKind + u32 族内子 id，5 字节 Copy）
//! * [`StreamId`]：数据面流标识（栈内联小字符串 SmolStr，≤23 字节零堆分配，类型隔离）
//! * [`StreamKey`]：语义流标识（23 字节全局确定性计算，双方无需协商即可本地推导）
//! * [`LinkId`]：接收端链路槽位（数值，0=main）
//! * [`StrategyId`]：策略标识枚举（Default / Passthrough / Chunked）
//! * [`TransferId`]：文件传输任务数值 ID（u32）
//! * [`MsgId`]：对等通道消息/便签数值 ID（u64）

pub use stross_proto::message::{
    EndpointId, LinkId, MsgId, NodeId, StrategyId, StreamId, StreamKey, TransferId,
};
