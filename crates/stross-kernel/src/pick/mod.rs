//! # pick 规则层：数据管道「装载/解读」语义（通信模式 v2，docs/comm-mode-v2.md §3.0）
//!
//! 心智模型：**原始数据 → 装载逻辑 → 传输数据 → 接收数据 → 解读逻辑 → 呈现数据**。
//! pick 规则（[`PickRule`]）是装载/解读的语义规则，目前两种：
//! **严格顺序（StrictOrdered）** / **严格即时（Realtime）**。
//!
//! 分层（两端对称，均由内核提供通用框架；端点只做「原始数据 ↔ 传输数据」
//! 的转化——编码/压缩/分块，经内核 trait 调用）：
//!
//! * [`interpret`]：**解读逻辑（接收侧）**——[`Interpreter`] trait +
//!   [`RealtimePacing`]（严格即时：低延迟、容忍丢帧）/ [`StrictOrdered`]
//!   （严格顺序：逐字节不丢）+ [`InterpretRegistry`]（按流 id 装载/索引，
//!   per-stream 实例，停止一条流只拆该流适配器）；
//! * [`load`]：**装载逻辑（发送侧）**——[`Loader`] trait。当前发送侧
//!   行为等价直通（无额外缓冲需求，plugin-architecture §9「不为抽象而
//!   抽象」）；Phase C 打 id / 调度发送节奏时在此扩展；
//! * [`buffer`]：解读内部机制——[`JitterBuffer`]（抖动缓冲：吸收网络抖动、
//!   按序产出；只服务有损/自适应路径，无损传输由 [`RealtimePacing`]
//!   直通处理，不经过缓冲）。

pub mod buffer;
pub mod interpret;
pub mod load;
pub mod manager;

pub use buffer::{JitterBuffer, JitterConfig, JitterStats};
pub use interpret::{Interpreter, RealtimePacing, StrictOrdered};
pub use load::{Loader, PassthroughLoader};
pub use manager::{ChannelKind, InterpretRegistry, StreamChannel};
pub use stross_proto::message::PickRule;
