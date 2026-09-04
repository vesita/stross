//! 对等文件与即时消息通道（Channel）：节点间全双工自由互传与聊天便签。
//!
//! 跑在无损传输（WS / QUIC）上，支持文本便签与大文件分块流式传输。
//!
//! **代码规范铁律**：严禁使用裸 `String` 作 key / id；传输任务与消息一律使用强类型
//! 数值新类型 [`TransferId`] 与 [`MsgId`]，会话索引使用强类型 [`crate::kernel::id::Id`]。

pub mod manager;
pub mod session;

pub use manager::ChannelManager;
pub use session::ChannelSession;
pub use stross_proto::message::{MsgId, TransferId};
pub use stross_view::channel::{ChannelEvent, ChannelStatus};
