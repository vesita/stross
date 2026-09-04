//! 内核 id 模块：统一从 [`stross_view::id`] 接入强类型 ID 单一真源。
//!
//! **代码规范铁律**：严禁直接使用裸 `String` / `&str` 作 key 或 id。
//! 全仓一律采用强类型新类型 / 枚举：[`NodeId`], [`StreamId`], [`StreamKey`],
//! [`LinkId`], [`EndpointId`], [`StrategyId`], [`TransferId`], [`MsgId`]。

pub use stross_view::id::*;

/// 内核会话/流标识向后兼容别名（已收敛至强类型 [`StreamId`]）。
pub type Id = StreamId;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_roundtrips_and_is_hashable() {
        let a = Id::from("sess-1");
        let b = Id::from("sess-1");
        let c = Id::from("sess-2");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.as_str(), "sess-1");
        assert_eq!(a.into_string(), "sess-1");
        // 用作 HashMap 键
        let mut m = std::collections::HashMap::new();
        m.insert(Id::from("k"), 42u8);
        assert_eq!(m.get(&Id::from("k")), Some(&42));
    }
}
