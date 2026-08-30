//! 订阅方编排（docs/endpoint-model-v2.md §4 的**订阅侧库接口**）。
//!
//! 分层（docs/layering-architecture.md）：订阅流程（本地接收准备 + 握手 +
//! watch/重试）是应用编排，收敛在 stross-app；壳层（CLI `endpoint` 子命令、
//! 未来 GUI 命令）只解析参数、调这里并格式化输出。
//!
//! v2（三层注册表）：订阅先拉取目录（把互联节点映射进统一注册表
//! 节点→端点→策略），再按 `(节点, 端点, 策略)` 解析策略组合、构建
//! [`SubscribeSpec`]（订阅端点生成依据）——自订与订其它互联节点同一套逻辑。
//!
//! * [`fetch_directory`]：L2 目录拉取（`GET /api/endpoints`，类型化）；
//! * [`subscribe_file`]：订阅一个文件端点并落盘（pull：连公开方中继 watch；
//!   push：本机建会话 + 自签凭证 + 锚定中继，等公开方出站推入后 watch 本机）。
//!
//! 握手原语在 [`crate::negotiator_client::request_grant`]（本模块复用；GUI
//! 命令「申请凭证 / 目录 / 订阅」同源调它）。

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::relay::client as relay_http;
use anyhow::Context;
use serde::Serialize;
use stross_proto::message::{Delivery, EndpointDir, ShareGrant, ShareRequest, SubscribeSpec};

use crate::Kernel;
use crate::bootstrap;
use crate::file_xfer::{ReceivedFile, receive_file};
use crate::negotiator_client;
use crate::watch::connect_watch;

/// 「流尚未出现」重试窗口：订阅方 watch 与公开方泵建流存在竞态
/// （授予响应先于流注册到达；pump 侧同样在等观看者，docs §5），
/// 写满该窗口内的重试即稳定收敛。
const STREAM_APPEAR_WINDOW: Duration = Duration::from_secs(9);
const RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// L2 目录（节点 + 设备 + 可订阅端点）。类型化：与 proto `EndpointDir`
/// 同一契约，服务端序列化逐字节一致。
pub async fn fetch_directory(host: &str, port: u16) -> anyhow::Result<EndpointDir> {
    relay_http::get_json(
        &format!("http://{host}:{port}/api/endpoints"),
        Duration::from_secs(3),
    )
    .await
    .context("拉取目录失败（对端 serve 的 --negotiator-port 是否一致？）")
}

/// 订阅结果（GUI 命令 / CLI 展示共用；JSON 序列化供前端消费）。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscribeOutcome {
    /// 公开方拍板后的方向（pull = 连公开方中继；push = 公开方推入本机）。
    pub delivery: Delivery,
    /// 提交给观看接收的流 id（pull = 公开方会话；push = 本机自签会话）。
    pub stream_id: String,
    /// 接收到的文件（文件端点半程：握手 → 接收 → 落盘）。
    pub received: ReceivedFile,
}

/// 媒体端点订阅结果（GUI 命令 / 未来 CLI 共用）：握手后交给既有接收链路
/// `start_receive(relay_url, stream_id)` 实际观看 / 播放。
/// （纯数据 DTO，定义收敛至 stross-types——应用契约层单一真源。）
pub use stross_types::MediaSubscribeOutcome;

/// 订阅远端媒体端点并返回观看入口（订阅驱动：只走 pull——连公开方中继
/// watch 取流，公开方在本地中继发布；无 push 出站路径）。
/// 订阅达成后公开方经端点驱动自动开推（docs/endpoint-model-v2.md §4：
/// 媒体端点 pull 推本机中继）。
pub async fn subscribe_media(
    app: &Arc<Kernel>,
    base: &Path,
    host: &str,
    port: u16,
    endpoint_id: &str,
    _delivery_wish: Option<Delivery>,
) -> anyhow::Result<MediaSubscribeOutcome> {
    let EndpointGrant { grant, node_id } =
        request_endpoint_grant(app, base, host, port, endpoint_id, None).await?;
    let delivery = grant.delivery.unwrap_or(Delivery::Pull);
    // v2：注册表 (节点, 端点, 策略) 解析策略组合（订阅端点生成依据；媒体
    // 播放由接收链路承担，策略随日志可查）
    let spec = build_subscribe_spec(app, host, port, endpoint_id, None, &grant, &node_id)?;
    tracing::info!(
        "订阅媒体端点 {endpoint_id}（节点 {node_id}，策略 {}: serialize={:?} pick={:?}）",
        spec.strategy.strategy_id,
        spec.strategy.serialize,
        spec.strategy.pick,
    );
    let (relay_url, stream_id) = match delivery {
        Delivery::Pull | Delivery::Both => {
            let relay = grant.relay.as_ref().ok_or_else(|| {
                anyhow::anyhow!("pull 授予缺少公开方中继地址（公开方未锚定中继）")
            })?;
            (
                format!("ws://{host}:{}", relay.ws_port),
                grant.view.stream_id.clone(),
            )
        }
        // 订阅驱动定稿（docs/endpoint-model-v2.md §4）：只走 pull，无 push
        Delivery::Push => {
            return Err(anyhow::anyhow!(
                "公开方授予了 push（对端版本偏差？——订阅驱动只走 pull）"
            ));
        }
    };
    Ok(MediaSubscribeOutcome {
        delivery,
        relay_url,
        stream_id,
    })
}

/// 订阅远端媒体端点并**作为观看者保持连接**（CLI 无头：连中继建立 watcher，
/// 循环读帧丢弃），直到对端断开 / 用户中断（Ctrl-C 断连即触发公开方
/// watchers→0 自动收尾）。P0-1 生命周期收尾的真机验证路径。
pub async fn subscribe_media_and_watch(
    app: &Arc<Kernel>,
    base: &Path,
    host: &str,
    port: u16,
    endpoint_id: &str,
    delivery_wish: Option<Delivery>,
) -> anyhow::Result<()> {
    let outcome = subscribe_media(app, base, host, port, endpoint_id, delivery_wish).await?;
    // 对「流尚未出现」重试（watcher 接入与公开方泵建流存在竞态，docs §5），
    // 其余错误（中途断开 / 采集失败）是真实失败，直接上报。
    let deadline = Instant::now() + STREAM_APPEAR_WINDOW;
    let session = loop {
        match connect_watch(&outcome.relay_url, &outcome.stream_id).await {
            Ok(s) => break s,
            Err(e) => {
                let msg = format!("{e}");
                if msg.contains("不存在") && Instant::now() < deadline {
                    tokio::time::sleep(RETRY_INTERVAL).await;
                    continue;
                }
                return Err(anyhow::anyhow!(e)
                    .context(format!("连接中继观看失败（流 {}）", outcome.stream_id)));
            }
        }
    };
    tracing::info!(
        "已订阅媒体端点（delivery={:?} relay={} stream={}）——Ctrl-C 断开即触发对方收尾",
        outcome.delivery,
        outcome.relay_url,
        outcome.stream_id,
    );
    // 保持 watcher：CLI 无显示，读帧丢弃；对端断开（Ok(None)）或异常（Err）即退出
    loop {
        match session.recv().await {
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => break,
        }
    }
    Ok(())
}

/// 订阅握手结果（[`request_endpoint_grant`] 的公共形态）。
struct EndpointGrant {
    grant: ShareGrant,
    /// 公开方节点 id（目录拉取；统一注册表 `(节点, 端点, 策略)` 查表键）。
    node_id: String,
}

/// 订阅握手：目录拉取（映射公开方入统一注册表）→ 身份 → POST 对端协商端点。
///
/// `subscribe_file`（文件端点）与 `subscribe_media`（媒体端点）共用；
/// 握手原语 [`negotiator_client::request_grant`]。
async fn request_endpoint_grant(
    app: &Arc<Kernel>,
    base: &Path,
    host: &str,
    port: u16,
    endpoint_id: &str,
    strategy_id: Option<String>,
) -> anyhow::Result<EndpointGrant> {
    // 1) 目录拉取 → 映射进统一注册表（节点 → 端点 → 策略 三层；
    //    docs/endpoint-model-v2.md §4「订阅分享注册表」）
    let mut node_id = String::new();
    if let Ok(dir) = fetch_directory(host, port).await {
        node_id = dir.node.device_id.clone();
        app.register_remote_directory(&dir, &format!("{host}:{port}"));
    }
    bootstrap::ensure_identity(app, base, crate::bootstrap::DEFAULT_NODE_NAME);
    let identity = app
        .device_identity()
        .ok_or_else(|| anyhow::anyhow!("身份未初始化"))?;

    // 订阅驱动定稿（docs/endpoint-model-v2.md §4）：只走 pull，无 push——
    // 不建本机会话/自签凭证/锚定中继；订阅方只连公开方中继 watch 取流。
    let req = ShareRequest {
        device_id: identity.device_id.clone(),
        device_name: identity.device_name.clone(),
        endpoint_id: Some(endpoint_id.to_string()),
        strategy_id,
        delivery_mode: Some(Delivery::Pull),
        relay_addr: None,
        share_token: None,
        media: vec![],
    };
    // 订阅握手（Public / Confirm+信任 自动签发；Confirm 首见需对端人工确认，
    // 超时由 negotiator_client 盖过挂起窗）
    let grant = negotiator_client::request_grant(host, port, &req)
        .await
        .context(format!(
            "订阅握手失败（端点 {endpoint_id}；Confirm 端点需对端 stross ctrl negotiator-list 确认）"
        ))?;
    Ok(EndpointGrant { grant, node_id })
}

/// 构建订阅规格（v2「订阅端点生成」依据，docs/endpoint-model-v2.md §2/§3）：
/// 从统一注册表 `registry[节点][端点][策略]` 解析策略组合（订阅方选定的
/// 策略 id，缺省 = 端点默认策略）。回退链：注册表 → 授予携带策略 →
/// 平铺 `pick_rule` 推导默认策略——保证旧对端 / 目录拉取失败时不阻断订阅。
fn build_subscribe_spec(
    app: &Arc<Kernel>,
    host: &str,
    _port: u16,
    endpoint_id: &str,
    strategy_id: Option<String>,
    grant: &ShareGrant,
    node_id: &str,
) -> anyhow::Result<SubscribeSpec> {
    let strategy = app
        .resolve_strategy(node_id, endpoint_id, strategy_id.as_deref())
        .or_else(|| grant.strategy.clone())
        .unwrap_or_else(|| stross_proto::message::EndpointStrategy {
            strategy_id: stross_proto::message::EndpointStrategy::DEFAULT_ID.into(),
            serialize: stross_proto::message::SerializeRule::Passthrough,
            pick: grant.pick_rule.unwrap_or_default(),
        });
    let relay_url = grant
        .relay
        .as_ref()
        .map(|r| format!("ws://{host}:{}", r.ws_port));
    Ok(SubscribeSpec {
        node_id: node_id.to_string(),
        endpoint_id: endpoint_id.to_string(),
        strategy_id,
        strategy,
        delivery: grant.delivery.unwrap_or(Delivery::Pull),
        stream_id: grant.view.stream_id.clone(),
        relay_url,
    })
}

/// 订阅远端文件端点并接收落盘（P1 文件端点完整闭环；订阅驱动只走 pull）。
/// v2：先经统一注册表解析 `(节点, 端点, 策略)` 策略组合构建 [`SubscribeSpec`]，
/// 再连公开方中继 watch 接收。
pub async fn subscribe_file(
    app: &Arc<Kernel>,
    base: &Path,
    host: &str,
    port: u16,
    endpoint_id: &str,
    _delivery_wish: Option<Delivery>,
    out: &Path,
) -> anyhow::Result<SubscribeOutcome> {
    let EndpointGrant { grant, node_id } =
        request_endpoint_grant(app, base, host, port, endpoint_id, None).await?;
    let delivery = grant.delivery.unwrap_or(Delivery::Pull);
    let spec = build_subscribe_spec(app, host, port, endpoint_id, None, &grant, &node_id)?;
    tracing::info!(
        "订阅文件端点 {endpoint_id}（节点 {node_id}，策略 {}: serialize={:?} pick={:?}）",
        spec.strategy.strategy_id,
        spec.strategy.serialize,
        spec.strategy.pick,
    );

    let received = match delivery {
        Delivery::Pull | Delivery::Both => {
            let relay = grant.relay.as_ref().ok_or_else(|| {
                anyhow::anyhow!("pull 授予缺少公开方中继地址（公开方未锚定中继）")
            })?;
            let watch_url = format!("ws://{host}:{}", relay.ws_port);
            receive_file_retry(&watch_url, &spec.stream_id, out).await?
        }
        // 订阅驱动定稿（docs/endpoint-model-v2.md §4）：只走 pull，无 push
        Delivery::Push => {
            return Err(anyhow::anyhow!(
                "公开方授予了 push（对端版本偏差？——订阅驱动只走 pull）"
            ));
        }
    };

    Ok(SubscribeOutcome {
        delivery,
        stream_id: spec.stream_id.clone(),
        received,
    })
}

/// 订阅远端文件端点并**经订阅端点生成**接收落盘（v2 订阅端框架路径，
/// docs/endpoint-model-v2.md §3）：注册表 `(节点, 端点, 策略)` → 生成订阅
/// 端点（[`FileReceiveEndpoint`]）→ 委托其 `subscribe`（端点自驱动，与分享端
/// `share` 同构）。与 [`subscribe_file`]（返回落盘结果）共用握手与规格构建；
/// 本路径面向「不需要同步结果」的自主订阅（后台接收 / 未来剪贴板同步等）。
pub async fn subscribe_file_via_endpoint(
    app: &Arc<Kernel>,
    base: &Path,
    host: &str,
    port: u16,
    endpoint_id: &str,
    out: &Path,
) -> anyhow::Result<()> {
    let EndpointGrant { grant, node_id } =
        request_endpoint_grant(app, base, host, port, endpoint_id, None).await?;
    let spec = build_subscribe_spec(app, host, port, endpoint_id, None, &grant, &node_id)?;
    app.subscribe_via_endpoint(app.clone(), &spec, Some(out))
        .map_err(|e| anyhow::anyhow!("订阅端点生成失败: {e}"))
}

/// 接收文件（对「流尚未出现」重试）：只对建流竞态重试，其它错误
/// （中途断开 / 文件不完整）是真实失败，直接上报。
///
/// `pub(crate)`：订阅端点（[`EndpointApp::receive_file`]）与 CLI
/// `subscribe_file` 共用此兜底——订阅端点生成路径同样要扛住
/// 「授予响应先于流注册到达」的竞态（docs/endpoint-model-v2.md §4）。
pub(crate) async fn receive_file_retry(
    watch_url: &str,
    stream_id: &str,
    out: &Path,
) -> anyhow::Result<ReceivedFile> {
    let deadline = Instant::now() + STREAM_APPEAR_WINDOW;
    loop {
        match receive_file(watch_url, stream_id, out).await {
            Ok(got) => return Ok(got),
            Err(e) => {
                if format!("{e:#}").contains("不存在") && Instant::now() < deadline {
                    tokio::time::sleep(RETRY_INTERVAL).await;
                    continue;
                }
                return Err(e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MicEndpoint, NoopUi, Platform, Probe, ShareNegotiator};
    use std::path::PathBuf;
    use stross_proto::message::Visibility;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("stross-sub-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn ok_probe() -> Probe {
        std::sync::Arc::new(|| Ok(()))
    }

    /// 进程内双节点闭环（订阅方侧走库接口 [`subscribe_file`]）：
    /// 公开方（中继 + 目录/握手端点 + 文件端点）↔ 订阅方
    /// （身份 → 握手 → watch → 落盘），文件逐字节一致。
    /// 覆盖：新订阅 API 全路径（pull），端点自驱动（share 契约）默认行为，
    /// 以及「流尚未出现」竞态重试的收敛。
    #[tokio::test]
    async fn subscribe_file_pull_roundtrip_in_process() {
        let dir_a = tmp_dir("a");
        let dir_b = tmp_dir("b");
        let out = tmp_dir("out");

        // —— 公开方节点 A ——
        let app_a = Arc::new(Kernel::new(Platform::Desktop));
        bootstrap::ensure_identity(&app_a, &dir_a, "stross");
        let _relay = app_a.start_relay_on(0, "stross").await.unwrap();
        let neg = ShareNegotiator::start(app_a.clone(), Arc::new(NoopUi), &dir_a, 0)
            .await
            .unwrap();
        let src = dir_a.join("payload.txt");
        let payload = b"subscriber roundtrip payload\n";
        std::fs::write(&src, payload).unwrap();
        let m = app_a
            .publish_file_endpoint(&src, Visibility::Public, Delivery::Pull)
            .unwrap();

        // —— 订阅方节点 B ——
        let app_b = Arc::new(Kernel::new(Platform::Desktop));
        let outcome = subscribe_file(
            &app_b,
            &dir_b,
            "127.0.0.1",
            neg.port,
            &m.endpoint_id,
            None, // 按端点声明（Pull）
            &out,
        )
        .await
        .expect("订阅文件应成功");
        assert_eq!(outcome.delivery, Delivery::Pull);
        assert_eq!(outcome.received.size, payload.len() as u64);
        let written = std::fs::read(&outcome.received.path).unwrap();
        assert_eq!(written, payload, "文件字节必须逐字节一致");
        // pull 流 id = 公开方签发的会话（非空即可；具体值取决于公开方内核）
        assert!(!outcome.stream_id.is_empty());

        neg.stop().await;
        let _ = std::fs::remove_dir_all(&dir_a);
        let _ = std::fs::remove_dir_all(&dir_b);
        let _ = std::fs::remove_dir_all(&out);
    }

    /// v2 订阅端点生成闭环（docs/endpoint-model-v2.md §3）：订阅方目录拉取 →
    /// 统一注册表映射（节点→端点→策略）→ 注册表解析策略组合 → 生成订阅端点
    /// （[`FileReceiveEndpoint`]）→ 委托 `subscribe` 落盘。
    /// 覆盖：三层注册表查表 + 订阅端点生成的完整框架路径（文件接收）。
    #[tokio::test]
    async fn subscribe_file_via_endpoint_roundtrip_in_process() {
        let dir_a = tmp_dir("va");
        let dir_b = tmp_dir("vb");
        let out = tmp_dir("vout");

        // —— 公开方节点 A（中继 + 目录/握手端点 + 文件端点）——
        let app_a = Arc::new(Kernel::new(Platform::Desktop));
        bootstrap::ensure_identity(&app_a, &dir_a, "stross");
        let _relay = app_a.start_relay_on(0, "stross").await.unwrap();
        let neg = ShareNegotiator::start(app_a.clone(), Arc::new(NoopUi), &dir_a, 0)
            .await
            .unwrap();
        let src = dir_a.join("via-endpoint.txt");
        let payload = b"subscribe endpoint generation roundtrip\n";
        std::fs::write(&src, payload).unwrap();
        let m = app_a
            .publish_file_endpoint(&src, Visibility::Public, Delivery::Pull)
            .unwrap();

        // —— 订阅方节点 B：订阅端点生成路径（fire-and-forget，轮询落盘）——
        let app_b = Arc::new(Kernel::new(Platform::Desktop));
        subscribe_file_via_endpoint(&app_b, &dir_b, "127.0.0.1", neg.port, &m.endpoint_id, &out)
            .await
            .expect("订阅端点生成应成功");
        // 生成端点自驱动接收：等待落盘（公开方等观看者接入 + 推送，秒级）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let received = loop {
            let got: Vec<_> = std::fs::read_dir(&out)
                .map(|it| it.filter_map(|e| e.ok()).collect::<Vec<_>>())
                .unwrap_or_default();
            if let Some(entry) = got.iter().find(|e| e.file_name() == "via-endpoint.txt") {
                break std::fs::read(entry.path()).expect("读落盘文件");
            }
            assert!(
                std::time::Instant::now() < deadline,
                "订阅端点应在超时内完成接收落盘"
            );
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        };
        assert_eq!(received, payload, "订阅端点接收的文件字节必须逐字节一致");

        neg.stop().await;
        let _ = std::fs::remove_dir_all(&dir_a);
        let _ = std::fs::remove_dir_all(&dir_b);
        let _ = std::fs::remove_dir_all(&out);
    }

    /// 进程内双节点：公开方通告麦克风端点（Public/Pull）+ 订阅方
    /// [`subscribe_media`] 握手 → 返回公开方中继 watch 入口与流 id。
    /// 覆盖：媒体端点订阅握手全路径（身份 → 握手 → 授予解析）。
    #[tokio::test]
    async fn subscribe_media_handshake_pull() {
        let dir_a = tmp_dir("ma");
        let dir_b = tmp_dir("mb");
        let app_a = Arc::new(Kernel::new(Platform::Desktop));
        app_a.seed_endpoint(Box::new(MicEndpoint::new("麦克风", ok_probe())));
        bootstrap::ensure_identity(&app_a, &dir_a, "stross");
        let relay = app_a.start_relay_on(0, "stross").await.unwrap();
        let neg = ShareNegotiator::start(app_a.clone(), Arc::new(NoopUi), &dir_a, 0)
            .await
            .unwrap();
        let m = app_a
            .publish_endpoint(
                "mic:builtin",
                stross_proto::message::Visibility::Public,
                Delivery::Pull,
                None,
                None,
            )
            .unwrap();

        let app_b = Arc::new(Kernel::new(Platform::Desktop));
        let outcome = subscribe_media(
            &app_b,
            &dir_b,
            "127.0.0.1",
            neg.port,
            &m.endpoint_id,
            None, // 按端点声明（Pull）
        )
        .await
        .expect("订阅媒体端点应成功");
        assert_eq!(outcome.delivery, Delivery::Pull);
        assert!(
            !outcome.stream_id.is_empty(),
            "pull 流 id = 公开方签发的会话（非空即可）"
        );
        assert!(
            outcome.relay_url.contains(&relay.port.to_string()),
            "pull watch 入口指向公开方中继: {}",
            outcome.relay_url
        );

        neg.stop().await;
        let _ = std::fs::remove_dir_all(&dir_a);
        let _ = std::fs::remove_dir_all(&dir_b);
    }
}
