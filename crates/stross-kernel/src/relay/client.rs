//! 中继 HTTP API 的**官方客户端**（与 server 侧 [`super::api`] 同 crate，
//! 响应契约单一真源；纯 raw TCP + serde_json，零新增依赖，平台无关）。
//!
//! 消费方：CLI `devices` / `adb status` 探测、`endpoint ls` 目录拉取、
//! 文件泵等观看者轮询（stross-app `file_xfer`）。任何一处解析 `/api/*`
//! 响应都应经本模块——禁止在壳层再手写 HTTP 客户端（docs/layering-architecture.md）。
//!
//! 兼容性：`/api/streams` 历史上有「裸数组」与「`{streams:[...]}`」两种形态
//! （前端双形态兼容），此处统一收敛为 [`StreamsResp`] 一次解析。

use std::time::Duration;

use anyhow::{Context, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use stross_proto::message::StreamInfo;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// `/api/info` 中继入口信息（各传输端口）。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InfoResp {
    /// HTTP/WS 端口。
    pub port: u16,
    /// SRT 推流/观看端口（随机分配）。
    pub srt_port: Option<u16>,
    /// QUIC 推流/观看端口（随机分配）。
    pub quic_port: Option<u16>,
}

/// `/api/streams` 响应：兼容裸数组（现行服务端形态）与
/// `{ "streams": [...] }` 包裹形态（历史兼容，前端同样双形态兼容）。
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum StreamsResp {
    Array(Vec<StreamInfo>),
    Object {
        #[serde(default)]
        streams: Vec<StreamInfo>,
    },
}

impl StreamsResp {
    /// 展开为流信息列表（两种形态统一）。
    pub fn list(self) -> Vec<StreamInfo> {
        match self {
            Self::Array(list) => list,
            Self::Object { streams } => streams,
        }
    }
}

/// 极简 HTTP GET（raw TCP 一发一收）：返回响应体原文。
/// `url` 接受 `ws://host:port/path` 或 `http://host:port/path` 基址+路径。
pub async fn http_get(url: &str, timeout: Duration) -> anyhow::Result<String> {
    let (host, port, path) = parse_url(url)?;
    let stream = tokio::time::timeout(
        timeout,
        tokio::net::TcpStream::connect((host.as_str(), port)),
    )
    .await
    .context(format!("连接失败 {url}"))??;
    stream.set_nodelay(true).ok();
    let mut stream = stream;
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\nAccept: */*\r\n\r\n"
    );
    tokio::time::timeout(timeout, stream.write_all(req.as_bytes()))
        .await
        .context("发送请求失败")??;
    let mut buf = Vec::new();
    tokio::time::timeout(timeout, stream.read_to_end(&mut buf))
        .await
        .context("读取响应失败")??;
    let body = String::from_utf8_lossy(&buf);
    Ok(body.split("\r\n\r\n").nth(1).unwrap_or("").to_string())
}

/// GET + JSON 反序列化（`T` 为响应契约类型）。
pub async fn get_json<T: DeserializeOwned>(url: &str, timeout: Duration) -> anyhow::Result<T> {
    let body = http_get(url, timeout).await?;
    serde_json::from_str(&body).context(format!("响应解析失败 {url}"))
}

/// POST JSON 请求体（raw TCP），按 `HTTP 状态 + {error}` 语义返回：
/// 200 → 反序列化 `T`；其它状态 → bail（优先提取响应体 `error` 字段）。
pub async fn post_json<T: DeserializeOwned, B: Serialize>(
    url: &str,
    body: &B,
    timeout: Duration,
) -> anyhow::Result<T> {
    let (host, port, path) = parse_url(url)?;
    let payload = serde_json::to_vec(body)?;
    let mut stream = tokio::time::timeout(
        timeout,
        tokio::net::TcpStream::connect((host.as_str(), port)),
    )
    .await
    .context(format!("连接失败 {url}"))??;
    stream.set_nodelay(true).ok();
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    tokio::time::timeout(timeout, stream.write_all(head.as_bytes()))
        .await
        .context("发送请求头失败")??;
    tokio::time::timeout(timeout, stream.write_all(&payload))
        .await
        .context("发送请求体失败")??;
    let mut buf = Vec::new();
    tokio::time::timeout(timeout, stream.read_to_end(&mut buf))
        .await
        .context("读取响应失败")??;
    let text = String::from_utf8_lossy(&buf);
    let (status_line, rest) = text
        .split_once("\r\n")
        .ok_or_else(|| anyhow::anyhow!("响应格式非法"))?;
    let body_json = rest.split("\r\n\r\n").nth(1).unwrap_or("");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("500")
        .parse::<u16>()
        .unwrap_or(500);
    if status != 200 {
        let msg = serde_json::from_str::<serde_json::Value>(body_json)
            .ok()
            .and_then(|v| v["error"].as_str().map(str::to_string))
            .unwrap_or_else(|| "未知错误".into());
        bail!("HTTP {status}: {msg}");
    }
    serde_json::from_str(body_json).context("响应 JSON 解析失败")
}

/// `/api/info` 探测（不可达返回 Err；调用方决定是否忽略）。
pub async fn info(host: &str, port: u16, timeout: Duration) -> anyhow::Result<InfoResp> {
    get_json(&format!("http://{host}:{port}/api/info"), timeout).await
}

/// `/api/streams` 拉取（流信息列表；两种响应形态统一展开）。
pub async fn streams(host: &str, port: u16, timeout: Duration) -> anyhow::Result<Vec<StreamInfo>> {
    let resp: StreamsResp = get_json(&format!("http://{host}:{port}/api/streams"), timeout).await?;
    Ok(resp.list())
}

/// 指定流的当前观看者数（流不存在 / 请求失败 = `None`；轮询方据此区分
/// 「还在等」与「探测失败」——等观看者逻辑把失败当 0 继续轮询）。
pub async fn stream_watchers(
    host: &str,
    port: u16,
    stream_id: &str,
    timeout: Duration,
) -> Option<u32> {
    let list = streams(host, port, timeout).await.ok()?;
    list.into_iter()
        .find(|s| s.stream_id == stream_id)
        .map(|s| s.watchers)
}

/// URL 拆解：`ws://` / `http://` 前缀（可见 base 兼收），路径缺省 `/`，
/// 端口缺省 80。
fn parse_url(url: &str) -> anyhow::Result<(String, u16, String)> {
    let rest = url
        .strip_prefix("ws://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or_else(|| anyhow::anyhow!("非法 HTTP 基址: {url}"))?;
    let (host_port, path) = match rest.split_once('/') {
        Some((hp, p)) => (hp, format!("/{p}")),
        None => (rest, "/".into()),
    };
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>().map_err(|_| anyhow::anyhow!("端口非法"))?,
        ),
        None => (host_port.to_string(), 80),
    };
    Ok((host, port, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ws_and_http_bases() {
        assert_eq!(
            parse_url("ws://192.168.1.5:18777/api/streams").unwrap(),
            ("192.168.1.5".into(), 18777, "/api/streams".into())
        );
        assert_eq!(
            parse_url("http://127.0.0.1:41355/api/negotiator/request").unwrap(),
            ("127.0.0.1".into(), 41355, "/api/negotiator/request".into())
        );
        // 无路径 → 根；无端口 → 80
        assert_eq!(
            parse_url("ws://example.local").unwrap(),
            ("example.local".into(), 80, "/".into())
        );
    }

    #[test]
    fn streams_resp_accepts_plain_array_and_object() {
        // 对齐历史兼容：/api/streams 可能是裸数组或 { "streams": [...] }
        let plain: StreamsResp = serde_json::from_str(
            r#"[{"streamId":"s1","title":"t","video":null,"audio":null,"startedAt":1,"watchers":2}]"#,
        )
        .unwrap();
        assert_eq!(plain.list()[0].watchers, 2);
        let obj: StreamsResp = serde_json::from_str(
            r#"{"streams":[{"streamId":"s1","title":"t","video":null,"audio":null,"startedAt":1,"watchers":2}]}"#,
        )
        .unwrap();
        assert_eq!(obj.list()[0].stream_id, "s1");
    }

    #[tokio::test]
    async fn unreachable_host_errors_not_panics() {
        // 端口 9 不可达：应 Err 而非 panic（raw TCP 路径的健壮性回归）
        assert!(
            http_get("ws://127.0.0.1:9/api/streams", Duration::from_millis(500))
                .await
                .is_err()
        );
        assert!(
            info("127.0.0.1", 9, Duration::from_millis(500))
                .await
                .is_err()
        );
    }
}
