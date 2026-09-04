//! 中继 HTTP API 的**官方客户端**（与 server 侧 [`super::api`] 同 crate，
//! 响应契约单一真源；基于 reqwest 标准库封装，支持连接复用与完善的 HTTP 语义）。
//!
//! 消费方：CLI `devices` / `adb status` 探测、`endpoint ls` 目录拉取、
//! 文件泵等观看者轮询（stross-app `file_xfer`）。任何一处解析 `/api/*`
//! 响应都应经本模块——禁止在壳层再手写 HTTP 客户端（docs/framework-v3.md）。
//!
//! 兼容性：`/api/streams` 历史上有「裸数组」与「`{streams:[...]}`」两种形态
//! （前端双形态兼容），此处统一收敛为 [`StreamsResp`] 一次解析。

use anyhow::{Context, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;
use std::time::Duration;
use stross_proto::message::StreamInfo;

/// `/api/info` 中继入口信息（各传输端口）。**契约单一真源**：与 server 侧
/// [`super::dto::RelayInfoResp`] 是同一结构（同一 crate），客户端直接复用，
/// 不再各自定义一份（此前两份字段相同、改动需同步两处）。
pub use super::dto::RelayInfoResp as InfoResp;

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

/// 进程内全局复用的 HTTP 客户端实例（连接池 + 零多余资源创建）。
static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .tcp_nodelay(true)
        .build()
        .expect("创建 reqwest 客户端失败")
});

fn http_client() -> &'static reqwest::Client {
    &HTTP_CLIENT
}
/// 规范化中继 URL：支持 `ws://` / `wss://` / `http://` / `https://` 格式转换为 HTTP URL。
fn normalize_url(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("ws://") {
        format!("http://{rest}")
    } else if let Some(rest) = url.strip_prefix("wss://") {
        format!("https://{rest}")
    } else {
        url.to_string()
    }
}

/// 校验 HTTP 状态码：非 2xx 时统一提取错误体（`{ "error": ... }` 或原文）并 bail。
///
/// `http_get` / `get_json` / `post_json` 共用——此前三段几乎相同的
/// 「非 2xx → 提取 error → bail」逻辑重复，收敛为本私有辅助（单一真源）。
async fn ensure_success(resp: reqwest::Response) -> anyhow::Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    let msg = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v["error"].as_str().map(str::to_string))
        .unwrap_or(body);
    bail!("HTTP {}: {}", status.as_u16(), msg)
}

/// 发起标准 HTTP GET 请求并返回文本响应体。
pub async fn http_get(url: &str, timeout: Duration) -> anyhow::Result<String> {
    let target = normalize_url(url);
    let resp = ensure_success(
        http_client()
            .get(&target)
            .timeout(timeout)
            .send()
            .await
            .with_context(|| format!("请求失败 {url}"))?,
    )
    .await?;
    resp.text()
        .await
        .with_context(|| format!("读取响应体失败 {url}"))
}

/// 发起 GET 请求并自动反序列化 JSON（`T` 为响应契约类型）。
pub async fn get_json<T: DeserializeOwned>(url: &str, timeout: Duration) -> anyhow::Result<T> {
    let target = normalize_url(url);
    let resp = ensure_success(
        http_client()
            .get(&target)
            .timeout(timeout)
            .send()
            .await
            .with_context(|| format!("请求失败 {url}"))?,
    )
    .await?;
    resp.json::<T>()
        .await
        .with_context(|| format!("响应 JSON 解析失败 {url}"))
}

/// POST JSON 请求体，并自动处理 HTTP 状态码与响应反序列化。
pub async fn post_json<T: DeserializeOwned, B: Serialize>(
    url: &str,
    body: &B,
    timeout: Duration,
) -> anyhow::Result<T> {
    let target = normalize_url(url);
    let resp = ensure_success(
        http_client()
            .post(&target)
            .json(body)
            .timeout(timeout)
            .send()
            .await
            .with_context(|| format!("请求失败 {url}"))?,
    )
    .await?;
    resp.json::<T>()
        .await
        .with_context(|| format!("响应 JSON 解析失败 {url}"))
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

/// 探测一个中继 HTTP 基址（`http://host:port`）是否可达：仅校验
/// `/api/streams` 端点（受控 / 普通中继都提供的只读端点）。不可达返回 `false`。
///
/// GUI「手动添加设备」校验地址用——壳层不再手写 `/api/*` 探测客户端
/// （docs/framework-v3.md：解析 `/api/*` 只允许在 stross-kernel）。
pub async fn probe_base(base: &str, timeout: Duration) -> bool {
    let url = format!("{}/api/streams", base.trim_end_matches('/'));
    get_json::<serde_json::Value>(&url, timeout).await.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_ws_and_http_urls() {
        assert_eq!(
            normalize_url("ws://192.168.1.5:18777/api/streams"),
            "http://192.168.1.5:18777/api/streams"
        );
        assert_eq!(
            normalize_url("wss://secure.local/api/info"),
            "https://secure.local/api/info"
        );
        assert_eq!(
            normalize_url("http://127.0.0.1:41355/api/negotiator/request"),
            "http://127.0.0.1:41355/api/negotiator/request"
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
        // 端口 9 不可达：应 Err 而非 panic（reqwest 错误传播正常）
        assert!(
            http_get("ws://127.0.0.1:9/api/streams", Duration::from_millis(500))
                .await
                .is_err()
        );
    }
}
