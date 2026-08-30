//! 端点 SPI 重导出（**定义单一真源在 stross-types**，docs/endpoint-model-v2.md
//! §3：内核约定特性、端点实现、内核只基于特性行动）。
//!
//! 本模块保持 `stross_endpoint::contract::*` 路径兼容；契约本体（
//! [`Endpoint`] / [`ShareEndpoint`] / [`SubscribeEndpoint`] /
//! [`MediaSourceEndpoint`] / [`EndpointApp`] / [`SubscribeCtx`] / 数据契约
//! [`StreamConfig`] 等）在 `stross_types::contract`。

pub use stross_types::contract::*;
pub use stross_types::impl_media_source_endpoint;
