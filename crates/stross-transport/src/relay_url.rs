//! 中继拨号地址的统一表示与解析。
//!
//! 中继地址在代码里曾以字符串散落多处手搓解析：`transport_for_url` 按前缀
//! 选传输、`watch` 判断是否拼 `/ws/watch`、`srt`/`quic` 各自 `strip_prefix`、
//! 应用层把 `ws://` 反推成 `http://` 基址。本模块收口这些规则：
//!
//! * [`RelayUrl::parse`]：`ws://host:port[/path]` / `wss://host:port[/path]` /
//!   `srt://host:port` / `quic://host:port`（无端口或未知 scheme → 解析失败）
//! * [`RelayUrl::push_url`] / [`RelayUrl::watch_url`]：拨号 URL 派生（一处定义）
//! * [`RelayUrl::base_http`]：`http://host:port/` 展示基址（级联/入口展示用）
//! * [`RelayUrl::transport`]：由 scheme 唯一确定的传输选择
//!
//! 构造侧（`auto_push_url` 等）也走 [`RelayUrl::ws`] / [`RelayUrl::srt`] /
//! [`RelayUrl::quic`]，保证"生成端"与"解析端"同一套格式。

use std::fmt;
use std::str::FromStr;

use stross_proto::message::TransportId;

/// 一个可拨号的中继地址。
///
/// scheme 决定传输：`ws|wss` → [`TransportId::Ws`]，`srt` → [`TransportId::Srt`]，
/// `quic` → [`TransportId::Quic`]。`ws` 可带路径（如 `/ws/push`）；UDP 系无路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayUrl {
    transport: TransportId,
    host: String,
    port: u16,
    /// ws 路径（如 `/ws/push`）；srt/quic 恒为 `None`。
    path: Option<String>,
}

impl RelayUrl {
    /// ws 地址构造（`path` 如 `/ws/push`；`None` = 基址）。
    pub fn ws(host: impl Into<String>, port: u16, path: Option<&str>) -> Self {
        Self {
            transport: TransportId::Ws,
            host: host.into(),
            port,
            path: path.map(str::to_string),
        }
    }

    /// srt 地址构造。
    pub fn srt(host: impl Into<String>, port: u16) -> Self {
        Self {
            transport: TransportId::Srt,
            host: host.into(),
            port,
            path: None,
        }
    }

    /// quic 地址构造。
    pub fn quic(host: impl Into<String>, port: u16) -> Self {
        Self {
            transport: TransportId::Quic,
            host: host.into(),
            port,
            path: None,
        }
    }

    /// 解析中继地址；未知 scheme / 缺端口 → `None`。
    pub fn parse(s: &str) -> Option<Self> {
        // 按 scheme 前缀匹配（wss 归 ws 系）；未知 scheme → None
        let (transport, rest) = s
            .strip_prefix("ws://")
            .map(|r| (TransportId::Ws, r))
            .or_else(|| s.strip_prefix("wss://").map(|r| (TransportId::Ws, r)))
            .or_else(|| s.strip_prefix("srt://").map(|r| (TransportId::Srt, r)))
            .or_else(|| s.strip_prefix("quic://").map(|r| (TransportId::Quic, r)))?;
        // 路径只属于 ws（如 `/ws/push`）；空路径视为无路径（split_once 会
        // 吞掉前导 `/`，补回）
        let (host_port, path) = match rest.split_once('/') {
            Some((hp, p)) if !p.is_empty() => (hp, Some(format!("/{p}"))),
            Some((hp, _)) => (hp, None),
            None => (rest, None),
        };
        let (host, port) = split_host_port(host_port)?;
        Some(Self {
            transport,
            host: host.to_string(),
            port,
            path,
        })
    }

    /// 传输 id（由 scheme 唯一决定）。
    pub const fn transport(&self) -> TransportId {
        self.transport
    }

    /// 主机（不含量词与端口）。
    pub fn host(&self) -> &str {
        &self.host
    }

    /// 端口。
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// 是否为 ws 系（`ws://` / `wss://`）。
    pub fn is_ws(&self) -> bool {
        self.transport == TransportId::Ws
    }

    /// 是否指向本机（`localhost` / 回环 IP）。
    pub fn is_loopback(&self) -> bool {
        self.host.eq_ignore_ascii_case("localhost")
            || self
                .host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    }

    /// 推流拨号地址：ws 无路径时补 `/ws/push`；srt/quic 原样。
    pub fn push_url(&self) -> String {
        match self.transport {
            TransportId::Ws => match &self.path {
                Some(p) => format!("ws://{}:{}{}", self.fmt_host(), self.port, p),
                None => format!("ws://{}:{}/ws/push", self.fmt_host(), self.port),
            },
            TransportId::Srt => format!("srt://{}:{}", self.fmt_host(), self.port),
            TransportId::Quic => format!("quic://{}:{}", self.fmt_host(), self.port),
            _ => unreachable!("RelayUrl 只承载 Ws/Srt/Quic"),
        }
    }

    /// 观看拨号地址：ws 派生 `/ws/watch?stream=` 端点（忽略已有路径——
    /// 观看总是从基址派生）；srt/quic 返回 `[`Self::push_url`]`（流由
    /// 带内 `Watch` 控制消息声明，URL 不带路径）。
    pub fn watch_url(&self, stream_id: &str) -> String {
        match self.transport {
            TransportId::Ws => format!(
                "ws://{}:{}/ws/watch?stream={}",
                self.fmt_host(),
                self.port,
                stream_id
            ),
            _ => self.push_url(),
        }
    }

    /// HTTP 基址（`http://host:port/`；局域网入口展示 / 级联代理基址）。
    pub fn base_http(&self) -> String {
        format!("http://{}:{}/", self.fmt_host(), self.port)
    }

    /// 单主机 HTTP 入口（`http://host:port/`；IPv6 补方括号）。
    pub fn http(host: &str, port: u16) -> String {
        Self::ws(host, port, None).base_http()
    }

    /// 本机局域网 HTTP 入口列表：每个局域网 IP 一条（多网卡全覆盖）；
    /// 无局域网 IP 时回退回环，保证至少一条可用。
    ///
    /// 收口「中继入口 / 观看 URL 列表」的构造（原散落在 app 与 relay 各处
    /// 手拼 `http://{ip}:{port}/` 并各自处理回退）。
    pub fn http_entries(port: u16) -> Vec<String> {
        let ips = crate::net::local_ips();
        let mut urls: Vec<String> = ips
            .iter()
            .map(|ip| Self::http(&ip.to_string(), port))
            .collect();
        if urls.is_empty() {
            urls.push(Self::http("127.0.0.1", port));
        }
        urls
    }

    /// 展示用主机：IPv6 补方括号（`[::1]`）。
    fn fmt_host(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        }
    }
}

/// 拆 `host:port`：方括号 IPv6（`[::1]:8777`）与普通主机名/IPv4 都支持。
fn split_host_port(hp: &str) -> Option<(&str, u16)> {
    if let Some(rest) = hp.strip_prefix('[') {
        // [::1]:8777
        let close = rest.find(']')?;
        let host = &rest[..close];
        let port = rest[close + 1..].strip_prefix(':')?.parse().ok()?;
        Some((host, port))
    } else {
        // host:port；末段为端口（IPv4 / 主机名）
        let (host, port) = hp.rsplit_once(':')?;
        port.parse().ok().map(|p| (host, p))
    }
}

impl FromStr for RelayUrl {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s).ok_or(())
    }
}

impl fmt::Display for RelayUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.push_url())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ws_base_and_push() {
        let base = RelayUrl::parse("ws://192.168.1.5:8777").expect("基址");
        assert_eq!(base.transport(), TransportId::Ws);
        assert_eq!(base.host(), "192.168.1.5");
        assert_eq!(base.port(), 8777);
        assert_eq!(base.push_url(), "ws://192.168.1.5:8777/ws/push");
        assert_eq!(
            base.watch_url("abc"),
            "ws://192.168.1.5:8777/ws/watch?stream=abc"
        );
        assert_eq!(base.base_http(), "http://192.168.1.5:8777/");

        // 带 push 路径：push_url 原样保留
        let push = RelayUrl::parse("ws://192.168.1.5:8777/ws/push").expect("push");
        assert_eq!(push.path.as_deref(), Some("/ws/push"));
        assert_eq!(push.push_url(), "ws://192.168.1.5:8777/ws/push");
        // watch 从基址派生，不受既有路径影响
        assert_eq!(
            push.watch_url("x"),
            "ws://192.168.1.5:8777/ws/watch?stream=x"
        );

        // wss 也归 ws 系
        let tls = RelayUrl::parse("wss://host:9000/").expect("wss");
        assert!(tls.is_ws());
        assert_eq!(tls.path, None, "空路径视为无路径");
    }

    #[test]
    fn parses_udp_transports() {
        let srt = RelayUrl::parse("srt://10.0.0.7:9000").expect("srt");
        assert_eq!(srt.transport(), TransportId::Srt);
        assert!(!srt.is_ws());
        assert_eq!(srt.push_url(), "srt://10.0.0.7:9000");
        assert_eq!(srt.watch_url("x"), "srt://10.0.0.7:9000");
        assert_eq!(srt.base_http(), "http://10.0.0.7:9000/");

        let quic = RelayUrl::parse("quic://10.0.0.7:9001").expect("quic");
        assert_eq!(quic.transport(), TransportId::Quic);
        assert_eq!(quic.push_url(), "quic://10.0.0.7:9001");
    }

    #[test]
    fn rejects_invalid() {
        assert!(
            RelayUrl::parse("http://host:8777").is_none(),
            "http 不可拨号"
        );
        assert!(RelayUrl::parse("ws://host").is_none(), "缺端口");
        assert!(RelayUrl::parse("host:8777").is_none(), "缺 scheme");
        assert!(RelayUrl::parse("ws://host:port").is_none(), "端口非数字");
        assert!(RelayUrl::parse("garbage").is_none());
        assert!(RelayUrl::parse("").is_none());
    }

    #[test]
    fn loopback_detection() {
        assert!(
            RelayUrl::parse("ws://127.0.0.1:8777")
                .unwrap()
                .is_loopback()
        );
        assert!(
            RelayUrl::parse("ws://localhost:8777")
                .unwrap()
                .is_loopback()
        );
        assert!(RelayUrl::parse("ws://[::1]:8777").unwrap().is_loopback());
        assert!(
            !RelayUrl::parse("ws://192.168.1.5:8777")
                .unwrap()
                .is_loopback()
        );
        assert!(
            !RelayUrl::parse("ws://relay.local:8777")
                .unwrap()
                .is_loopback()
        );
    }

    #[test]
    fn ipv6_bracketed_and_display() {
        let url = RelayUrl::parse("srt://[::1]:9000").expect("ipv6");
        assert_eq!(url.host(), "::1");
        assert_eq!(url.port(), 9000);
        assert!(url.is_loopback());
        assert_eq!(url.to_string(), "srt://[::1]:9000");
    }

    #[test]
    fn constructors_match_parse_roundtrip() {
        let a = RelayUrl::ws("192.168.1.5", 8777, Some("/ws/push"));
        assert_eq!(
            RelayUrl::parse(&a.to_string()).expect("roundtrip"),
            a,
            "构造 → 解析应一致"
        );
        let b = RelayUrl::srt("127.0.0.1", 9000);
        assert_eq!(RelayUrl::parse(&b.to_string()).expect("roundtrip"), b);
        let c = RelayUrl::quic("127.0.0.1", 9001);
        assert_eq!(RelayUrl::parse(&c.to_string()).expect("roundtrip"), c);
    }

    #[test]
    fn http_entry_helpers() {
        assert_eq!(
            RelayUrl::http("192.168.1.5", 8777),
            "http://192.168.1.5:8777/"
        );
        assert_eq!(
            RelayUrl::http("::1", 8777),
            "http://[::1]:8777/",
            "IPv6 补方括号"
        );
        let entries = RelayUrl::http_entries(8777);
        assert!(!entries.is_empty(), "至少一条回退入口");
        assert!(
            entries
                .iter()
                .all(|u| u.starts_with("http://") && u.ends_with(":8777/")),
            "全部为合法入口: {entries:?}"
        );
    }
}
