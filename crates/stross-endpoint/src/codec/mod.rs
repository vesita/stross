//! 数据处理辅助：编码流切帧工具（源与还原两侧共用）。
//!
//! * [`nal`]：H.264 Annex-B 流切帧（[`AnnexBSplitter`] / [`AccessUnitBuilder`]，
//!   含 SPS 分辨率解析 / CSD 提取）
//! * [`adts`]：AAC ADTS 流切帧（[`AdtsSplitter`]）

pub mod adts;
pub mod nal;
