//! 内部宏：为「无载荷 + serde `camelCase` rename」的枚举生成 `as_str` / `from_wire`。
//!
//! # 动机（消除 as_str/from_wire 双真相源漂移）
//!
//! `MediaKind` / `Delivery` / `EndpointState` 这类枚举，常用一个手写 `as_str()`
//! 返回 wire 字符串（供日志/命令参数/占位），又一个手写 `from_wire()` 反向解析。
//! 它们与 serde 的 `#[serde(rename_all = "camelCase")]` 是**两套对 wire 字符串的
//! 维护**，只能靠测试保证不漂移（历史上 `sysaudio` vs `systemAudio` 曾踩过）。
//!
//! 本宏让这两个函数**从一个 wire 表生成**——改一处即两侧同步，编译期就把
//! 「as_str 与 from_wire 不一致」变成不可能，真正收敛为单一真源。
//!
//! # 用法
//!
//! ```ignore
//! use stross_proto::message::define_wire_strings;
//! #[derive(Serialize, Deserialize)] #[serde(rename_all = "camelCase")]
//! enum Delivery { Pull, Push, Both }
//! define_wire_strings! {
//!     Delivery: Pull => "pull", Push => "push", Both => "both"
//! }
//! ```
//!
//! 生成：
//! * `pub const fn as_str(&self) -> &'static str`——按 wire 表 match 变体；
//! * `pub fn from_wire(s: &str) -> Option<Self>`——反向 match wire 表，未知值 `None`。
//!
//! # 限制
//!
//! 仅适用**无载荷**的 C-style 变体。带载荷变体（如 `Visibility::Private { nodes }`）
//! 因 `from_wire` 无法重建载荷、且构造形态各异的变体，保持手写实现。

/// 从一个 `Variant => "wire"` 表同时生成 `as_str` / `from_wire`。
///
/// 生成在 `impl <Type>` 内；要求枚举已实现 `Clone`（`from_wire` 用）与
/// `PartialEq`（解析返回值），以及 serde `Serialize`/`Deserialize` 与
/// `#[serde(rename_all = "camelCase")]`（保持一致，本宏**不**改变 wire 名字）。
macro_rules! define_wire_strings {
    ($ty:ty : $( $variant:ident => $wire:literal ),+ $(,)?) => {
        /// wire 字符串（camelCase；与 serde 序列化一致，单一真源）。
        pub const fn as_str(&self) -> &'static str {
            match self {
                $( Self::$variant => $wire, )+
            }
        }

        /// 从 wire 字符串解析（与 [`Self::as_str`] 互逆；未知值返回 `None`）。
        pub fn from_wire(s: &str) -> Option<Self> {
            match s {
                $( $wire => Some(Self::$variant), )+
                _ => None,
            }
        }
    };
}

/// 对每个 wire 字符串做编译/行为双向一致性校验（供测试用）。
///
/// `as_str` 由宏生成后与 serde 天然一致，本校验再对**每个变体的 serde JSON
/// 表示**断言等于 `"wire"`（含引号），并断言 `from_wire(as_str())` 回环成功。
/// 新增变体时漏改 serde / 宏表都会在此报警。仅测试构建需要，故随 `cfg(test)` 编译。
///
/// 首个参数是生成的测试函数名——同一模块内多次调用（对多个枚举校验）须唯一。
#[cfg(test)]
macro_rules! assert_wire_strings_consistent {
    ($name:ident : $ty:ty ; $( $variant_expr:expr => $wire:literal ),+ $(,)?) => {
        #[test]
        fn $name() {
            for (v, wire) in [
                $( ($variant_expr, $wire), )+
            ] {
                assert_eq!(
                    serde_json::to_string(&v).unwrap(),
                    format!("\"{wire}\""),
                    "serde 序列化必须与 as_str 单一真源一致"
                );
                assert_eq!(<$ty>::from_wire(wire), Some(v), "from_wire 必须回环 as_str");
            }
        }
    };
}

#[cfg(test)]
pub(crate) use assert_wire_strings_consistent;
pub(crate) use define_wire_strings;
