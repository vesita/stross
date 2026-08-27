//! 订阅方编排（docs/endpoint-model.md §5 的**订阅侧库接口**）。
//!
//! 分层（docs/layering-architecture.md）：订阅流程（本地接收准备 + 握手 +
//! watch/重试）是应用编排，收敛在 stross-app；壳层（CLI `endpoint` 子命令、
//! 未来 GUI 命令）只解析参数、调这里并格式化输出。
//!
//! * [`fetch_directory`]：L2 目录拉取（`GET /api/endpoints`，类型化）；
//! * [`subscribe_file`]：订阅一个文件端点并落盘（pull：连公开方中继 watch；
//!   push：本机建会话 + 自签凭证 + 锚定中继，等公开方出站推入后 watch 本机）。
//!
//! 握手超时盖过 Confirm 挂起窗（60s）：首见 Confirm 端点要求人工确认，
//! 读超时必须比挂起窗长，否则首见订阅会被误报失败。

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use stross_core::net;
use stross_core::relay::client as relay_http;
use stross_proto::message::{Delivery, EndpointDir, MediaKind, ShareGrant, ShareRequest};

use crate::app::StrossApp;
use crate::bootstrap;
use crate::file_xfer::{ReceivedFile, receive_file};

/// 订阅握手 / 目录拉取的协商端点超时（必须盖过 Confirm 挂起窗 60s）。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(70);
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

/// 订阅结果。
#[derive(Debug)]
pub struct SubscribeOutcome {
    /// 公开方拍板后的方向（pull = 连公开方中继；push = 公开方推入本机）。
    pub delivery: Delivery,
    /// 提交给观看接收的流 id（pull = 公开方会话；push = 本机自签会话）。
    pub stream_id: String,
    /// 接收到的文件（文件端点半程：握手 → 接收 → 落盘）。
    pub received: ReceivedFile,
}

/// 订阅远端文件端点并接收落盘（P1 文件端点完整闭环）。
pub async fn subscribe_file(
    app: &Arc<StrossApp>,
    base: &Path,
    host: &str,
    port: u16,
    endpoint_id: &str,
    delivery_wish: Option<Delivery>,
    out: &Path,
) -> anyhow::Result<SubscribeOutcome> {
    bootstrap::ensure_identity(app, base);
    let identity = app
        .device_identity()
        .ok_or_else(|| anyhow::anyhow!("身份未初始化"))?;

    // push 意向：先建本机会话 + 自签凭证 + 锚定本机中继（docs §5 凭证修正）
    let local = if matches!(delivery_wish, Some(Delivery::Push)) {
        Some(prepare_local_receiver(app).await?)
    } else {
        None
    };
    let req = ShareRequest {
        device_id: identity.device_id.clone(),
        device_name: identity.device_name.clone(),
        endpoint_id: Some(endpoint_id.to_string()),
        delivery_mode: delivery_wish,
        relay_addr: local.as_ref().map(|l| l.relay_addr.clone()),
        share_token: local.as_ref().map(|l| l.share_token.clone()),
        media: vec![],
    };
    // 订阅握手（Public / Confirm+信任 自动签发；Confirm 首见需对端人工确认，
    // 超时 70s 盖过挂起窗）
    let url = format!("http://{host}:{port}/api/negotiator/request");
    let grant: ShareGrant = relay_http::post_json(&url, &req, HANDSHAKE_TIMEOUT)
        .await
        .context(format!(
            "订阅握手失败（端点 {endpoint_id}；Confirm 端点需对端 stross ctrl negotiator-list 确认）"
        ))?;
    let delivery = grant.delivery.unwrap_or(Delivery::Pull);

    let received = match delivery {
        Delivery::Pull => {
            let relay = grant.relay.as_ref().ok_or_else(|| {
                anyhow::anyhow!("pull 授予缺少公开方中继地址（公开方未锚定中继）")
            })?;
            let watch_url = format!("ws://{host}:{}", relay.ws_port);
            receive_file_retry(&watch_url, &grant.view.stream_id, out).await?
        }
        Delivery::Push => {
            let l = local
                .ok_or_else(|| anyhow::anyhow!("push 授予但本机未准备接收（自签凭证缺失）"))?;
            let watch_url = format!("ws://127.0.0.1:{}", l.relay_port);
            receive_file_retry(&watch_url, &l.stream_id, out).await?
        }
        Delivery::Both => unreachable!("公开方已定稿，授予不含 Both"),
    };

    Ok(SubscribeOutcome {
        delivery,
        stream_id: grant.view.stream_id,
        received,
    })
}

/// 接收文件（对「流尚未出现」重试）：只对建流竞态重试，其它错误
/// （中途断开 / 文件不完整）是真实失败，直接上报。
async fn receive_file_retry(
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

/// push 模式本机准备：锚定受控中继 + 建会话 + 自签一次性凭证。
struct LocalReceiver {
    relay_addr: String,
    relay_port: u16,
    /// 本机自签会话（= 数据面流 id；公开方出站推的就是它）。
    stream_id: String,
    share_token: String,
}

async fn prepare_local_receiver(app: &Arc<StrossApp>) -> anyhow::Result<LocalReceiver> {
    let relay = app.start_relay_on(0).await?;
    let view =
        app.issue_share_token_for("订阅接收文件".into(), vec![MediaKind::File], Some(600))?;
    let ip = net::advertise_ip();
    Ok(LocalReceiver {
        relay_addr: format!("ws://{ip}:{}", relay.port),
        relay_port: relay.port,
        stream_id: view.stream_id,
        share_token: view.token,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NoopUi, Platform, ShareNegotiator, install_endpoint_driver};
    use std::path::PathBuf;
    use stross_proto::message::Visibility;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("stross-sub-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 进程内双节点闭环（订阅方侧走库接口 [`subscribe_file`]）：
    /// 公开方（中继 + 目录/握手端点 + 文件端点 + 驱动）↔ 订阅方
    /// （身份 → 握手 → watch → 落盘），文件逐字节一致。
    /// 覆盖：新订阅 API 全路径（pull），驱动默认行为（bootstrap 收敛后
    /// 安装即开推），以及「流尚未出现」竞态重试的收敛。
    #[tokio::test]
    async fn subscribe_file_pull_roundtrip_in_process() {
        let dir_a = tmp_dir("a");
        let dir_b = tmp_dir("b");
        let out = tmp_dir("out");

        // —— 公开方节点 A ——
        let app_a = Arc::new(StrossApp::new(Platform::Desktop));
        bootstrap::ensure_identity(&app_a, &dir_a);
        let _relay = app_a.start_relay_on(0).await.unwrap();
        let neg = ShareNegotiator::start(app_a.clone(), Arc::new(NoopUi), &dir_a, 0)
            .await
            .unwrap();
        // 订阅驱动：默认行为由 bootstrap::start 安装；此处等价手动安装
        install_endpoint_driver(&app_a);
        let src = dir_a.join("payload.txt");
        let payload = b"subscriber roundtrip payload\n";
        std::fs::write(&src, payload).unwrap();
        let m = app_a
            .publish_file_endpoint(&src, Visibility::Public, Delivery::Pull)
            .unwrap();

        // —— 订阅方节点 B ——
        let app_b = Arc::new(StrossApp::new(Platform::Desktop));
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
}
