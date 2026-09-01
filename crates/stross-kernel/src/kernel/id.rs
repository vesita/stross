//! 内核 id 新类型：**禁止直接用 `String` / `&str` 作 id**。
//!
//! 内核内部（会话 / 推流 / 接收 / 端点）的 id 统一用 [`Id`] 承载，底层是
//! [`smol_str::SmolStr`]（短字符串栈内联，`clone` 零分配），既避免散落的
//! `String` 堆分配与重复 `to_string()`，也让「这是 id 而不是普通字符串」在
//! 类型层面显式。
//!
//! 边界约定：
//! * **壳层 API**（`pub fn ...(&str)`）保持在 `&str`——壳层仍传字符串，入口
//!   处 `Id::from(s)` 转换，不改壳层调用点；
//! * **线上 / 序列化类型**（`stross-proto` 的 `ShareToken` / `StreamInfo` /
//!   `Session` 等字段为 `String`）在写入处 `into_string()`，读取处转回 `Id`。
//!
//! 说明：内核文档约定「session_id 与 stream_id 合一」（会话生命周期 = 流的
//! 生命周期，D4），故二者共用同一 `Id` 类型；node / endpoint / link 的 id
//! 语义不同，如需更强类型隔离可在后续拆分为独立新类型（本模块留此扩展点）。

use smol_str::SmolStr;

/// 内核 id 新类型（底层小字符串）。
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct Id(SmolStr);

impl Id {
    /// 取底层字符串引用。
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// 消耗自身，转为拥有的 `String`（写入线序/序列化字段用）。
    pub fn into_string(self) -> String {
        self.0.to_string()
    }
}

impl From<&str> for Id {
    fn from(s: &str) -> Self {
        Self(SmolStr::new(s))
    }
}

impl From<&String> for Id {
    fn from(s: &String) -> Self {
        Self(SmolStr::new(s.as_str()))
    }
}

impl From<String> for Id {
    fn from(s: String) -> Self {
        Self(SmolStr::new(s))
    }
}

impl std::fmt::Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::ops::Deref for Id {
    type Target = str;
    fn deref(&self) -> &str {
        self.0.as_str()
    }
}

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

    #[test]
    fn id_short_string_clone_is_cheap_and_eq() {
        let a = Id::from("stream-abc");
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn id_from_owned_string() {
        let s = String::from("endpoint-9");
        let id: Id = s.into();
        assert_eq!(id.as_str(), "endpoint-9");
    }
}
