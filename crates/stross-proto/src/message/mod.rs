//! 控制消息（JSON 文本帧）与协议载荷类型。
//!
//! 协议 v2 在原有会话控制（Hello/Bye/Welcome/Ready/Error/Info）基础上，
//! 增加**能力协商**与**路由控制**（见 docs/plugin-architecture.md §5.2）：
//! 推流端/观看端上报能力（`Capabilities`），会话建立时协商传输与编解码
//! （`Offer`/`Answer`），会话存续期间可动态改道（`Route`）。
//!
//! 模块划分（按域，公开路径经下方重导出保持兼容）：
//!
//! * [`ids`]：基础标识符枚举（传输 / 编解码 / 可靠性 / 能力 / 媒体 / 角色）
//! * [`capability`]：能力描述与协商 / 路由控制类型
//! * [`stream`]：流信息（推流声明与流列表共用）
//! * [`control`]：[`ControlMessage`] 控制消息
//! * [`discovery`]：mDNS 发现能力引导（单 key JSON）
//! * [`endpoint`]：端点框架（节点 → 设备 → 端点，见 docs/endpoint-model.md）
//! * [`token`]：一次性接入凭证

pub mod capability;
pub mod control;
pub mod discovery;
pub mod endpoint;
pub mod ids;
pub mod negotiator;
pub mod platform;
pub mod stream;
pub mod token;

pub use capability::{CapabilityDescriptor, RoutePath, SessionEventKind, TransportOffer};
pub use control::ControlMessage;
pub use discovery::{DiscoveryInfo, TXT_KEY_DISCOVERY};
pub use endpoint::{
    Delivery, EndpointManifest, EndpointState, EndpointSummary, FileMeta, TransportPreference,
    Visibility,
};
pub use ids::{CapabilityKind, CodecId, MediaKind, ReliabilityProfile, RoleId, TransportId};
pub use negotiator::{
    EndpointDir, EndpointNode, RelayAddr, ShareGrant, ShareRequest, ShareTokenView,
};
pub use platform::Platform;
pub use stream::{StreamInfo, TrackInfo};
pub use token::ShareToken;
