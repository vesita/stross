//! 数据处理辅助：像素格式转换与缩放。
//!
//! * [`yuv`]：YUV420p ↔ RGBA 转换与缩放（`yuv420_to_rgba_scaled`）、
//!   Wayland 采集的 BGRA → YUV420p 转换（[`yuv::bgra_to_yuv420p`]）

pub mod yuv;
