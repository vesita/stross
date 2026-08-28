//! # stross-bridge —— 平台适应桥接层
//!
//! [`stross_kernel`] 内核零路径 / 零 OS 调用 / 零平台分支（红线）；一切
//! 平台有关的知识（数据目录在哪、本机叫什么、哪个平台上有哪些设备能力）
//! 都收在这里，供壳层（CLI / GUI）调用后**注入**内核：
//!
//! * [`paths`]：数据目录解析（XDG/HOME 回退链，单一真源）
//! * [`hostname`]：本机主机名（OS 调用收敛点）
//! * [`devices`]：平台设备静态枚举（桌面 / Android 能力清单）与注入
//!
//! 桥接层只产出**参数**，不持有状态：内核拿到 base_dir / hostname /
//! 设备清单后自行运作，壳层无需再复制任何解析逻辑。

pub mod devices;
pub mod hostname;
pub mod paths;

pub use devices::{platform_devices, seed_platform_devices};
pub use hostname::hostname_or;
pub use paths::data_dir;
