//! 中继服务器：接收推流，向观看者广播。
//!
//! 借鉴 [MediaMTX](https://github.com/bluenviron/mediamtx) 的"推流端 → 中继 → 观看端"模型，
//! 并支持**级联代理**（转发链/树）：中继可将上游中继的流拉到本地广播，
//! 观看端只需连接最近的中继（见 [`RelayState::start_proxy`]，对齐 MoQ 的中继链语义）。
//!
//! * `GET /ws/push`：推流端 WebSocket（先发 `Hello`，再发二进制媒体帧）
//! * `GET /ws/watch?stream=<id>`：观看端 WebSocket（收到 `Ready` 后收帧）
//! * `GET /api/streams`：流列表（含本地推流与代理流）
//! * `POST /api/proxy`：请求代理上游中继的流（级联）
//! * `GET /`：内嵌的观看端页面
//!
//! 数据面转发（[`data_plane::handle_push`] / [`data_plane::handle_watch`]）只依赖
//! [`Transport`](crate::transport::Transport) 抽象，不感知具体传输；
//! 当前经 [`WsTransport`](crate::transport::ws::WsTransport) 从 HTTP 升级处接入
//! （见 docs/framework-v3.md §4）。
//!
//! 观看端接入时机：视频只在关键帧（IDR）后开始转发（ffmpeg 已在关键帧前重复
//! SPS/PPS，因此等待关键帧即可）；音频 ADTS 自带配置，可直接转发。
//!
//! 模块划分：
//!
//! * [`data_plane`]：数据面转发（传输无关；HTTP / SRT / QUIC 共用）
//! * [`state`]：共享状态（流表 / 代理表 / 受控授权 / 事件广播）
//! * [`server`]：生命周期（[`RelayHandle`] / [`RelayServer`]）
//! * [`api`]：HTTP 路由 / REST API / WebSocket 升级 / WebRTC 信令（utoipa 声明）
//! * [`dto`]：REST API 的请求/响应 DTO（`ToSchema`）
//! * [`client`]：中继 HTTP API 的官方客户端（`/api/info`、`/api/streams`、POST JSON）
//! * [`peers`]：局域网设备发现缓存（[`PeerInfo`]，feature `discovery`）

mod api;
mod data_plane;
mod dto;
mod peers;
mod server;
mod state;

pub mod client;

pub use peers::PeerInfo;
pub use server::{DEFAULT_PORT, GUI_PORT, RelayHandle, RelayServer};
pub use state::{RelayEvent, RelayState, ShareTokenValidator};

// 供同模块族（data_plane 等）访问流表条目内部结构。
pub(crate) use state::StreamEntry;

#[cfg(test)]
mod tests {
    use super::*;
    use stross_proto::frame::{CODEC_H264, FLAG_KEYFRAME, Frame, TRACK_VIDEO};
    use stross_proto::message::{ControlMessage, ReliabilityProfile};

    use crate::transport::ws::WsTransport;
    use crate::transport::{DataSession, PeerAddr, SessionPacket, SessionParams, Transport};
    use crate::watch::connect_watch;
    use tokio::time::Duration;

    /// 在 `base`（ws://host:port）中继上建流并发送一个关键帧（等 Welcome 确认流已注册）。
    /// 返回推流会话：调用方必须持有它直到断言结束（drop 会断开并删流）。
    async fn push_keyframe(base: &str, stream_id: &str) -> Box<dyn DataSession> {
        let transport = WsTransport::new();
        let peer = PeerAddr {
            transport: stross_proto::message::TransportId::Ws,
            addr: format!("{base}/ws/push"),
        };
        let params = SessionParams {
            session_id: stream_id.into(),
            profile: ReliabilityProfile::Lossless,
        };
        let push = transport.connect(&peer, &params).await.unwrap();
        push.send(SessionPacket::Control(ControlMessage::Hello {
            stream_id: stream_id.into(),
            title: "级联测试流".into(),
            video: None,
            audio: None,
            share_token: None,
        }))
        .await
        .unwrap();
        // 等 Welcome，确保流已建立（避免观看/代理先到报「流不存在」）
        loop {
            match tokio::time::timeout(Duration::from_secs(5), push.recv()).await {
                Ok(Ok(Some(SessionPacket::Control(ControlMessage::Welcome { .. })))) => break,
                Ok(Ok(Some(_))) => continue,
                Ok(Ok(None)) => panic!("推流连接提前关闭"),
                Ok(Err(e)) => panic!("推流 recv 错误: {e}"),
                Err(_) => panic!("等 Welcome 超时"),
            }
        }
        push.send(SessionPacket::Media(Frame::new(
            TRACK_VIDEO,
            CODEC_H264,
            FLAG_KEYFRAME,
            0,
            vec![0x65, 0x88, 0x00, 0x01],
        )))
        .await
        .unwrap();
        push
    }

    /// 从 `base` 中继观看 `stream_id`，直到收到关键帧（带超时）。
    async fn watch_until_keyframe(base: &str, stream_id: &str) {
        let session = connect_watch(base, stream_id)
            .await
            .expect("watch 连接应成功");
        loop {
            match tokio::time::timeout(Duration::from_secs(5), session.recv()).await {
                Ok(Ok(Some(SessionPacket::Media(f)))) if f.header.is_keyframe() => break,
                Ok(Ok(Some(_))) => continue,
                Ok(Ok(None)) => panic!("观看连接提前关闭"),
                Ok(Err(e)) => panic!("观看 recv 错误: {e}"),
                Err(_) => panic!("收关键帧超时"),
            }
        }
    }

    /// 级联拓扑：R1 推流 → R2 代理 R1 的流 → 观看端连 R2 收到关键帧。
    #[tokio::test]
    async fn relay_proxies_remote_stream() {
        let r1 = RelayServer::start(0).await.unwrap();
        let r2 = RelayServer::start(0).await.unwrap();
        let base1 = format!("ws://127.0.0.1:{}", r1.port);
        let base2 = format!("ws://127.0.0.1:{}", r2.port);

        let _push1 = push_keyframe(&base1, "cascade-1").await;

        // R2 代理 R1 的流（调用方未知上游 info，走占位）
        let id = r2.state().start_proxy(&base1, "cascade-1", None).unwrap();
        assert_eq!(id, "cascade-1");
        // 代理流立即出现在 R2 的流列表
        assert!(r2.streams().iter().any(|s| s.stream_id == "cascade-1"));

        // 观看端连 R2 收到关键帧（经 R2 → R1 级联转发）
        watch_until_keyframe(&base2, "cascade-1").await;

        // 代理流信息（/api/streams 展示用）
        let info = r2
            .streams()
            .into_iter()
            .find(|s| s.stream_id == "cascade-1")
            .expect("代理流应存在于 R2 流表");
        assert_eq!(info.title, "cascade-1");

        r1.stop().await;
        r2.stop().await;
    }

    /// 代理不存在的流：任务失败后本地占位流被清理。
    #[tokio::test]
    async fn proxy_of_missing_stream_cleans_up() {
        let r1 = RelayServer::start(0).await.unwrap();
        let r2 = RelayServer::start(0).await.unwrap();
        let base1 = format!("ws://127.0.0.1:{}", r1.port);

        r2.state()
            .start_proxy(&base1, "no-such-stream", None)
            .unwrap();
        // 等拉取任务失败并清理（connect_watch 会收到「流不存在」错误）
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            r2.streams().iter().all(|s| s.stream_id != "no-such-stream"),
            "代理不存在的流应自动清理"
        );

        r1.stop().await;
        r2.stop().await;
    }

    /// 同名冲突：本地已有推流时拒绝代理。
    #[tokio::test]
    async fn proxy_conflicts_with_local_stream() {
        let r2 = RelayServer::start(0).await.unwrap();
        let base1 = format!("ws://127.0.0.1:{}", r2.port);

        let _push2 = push_keyframe(&base1, "local-1").await;
        let err = r2.state().start_proxy("ws://127.0.0.1:1", "local-1", None);
        assert!(err.is_err(), "本地已有同名流时应拒绝代理");

        r2.stop().await;
    }

    /// 上游推流结束后，代理任务退出、本地代理流被清理（覆盖 sender clone 悬挂 bug）。
    #[tokio::test]
    async fn proxy_cleans_up_when_upstream_stream_ends() {
        let r1 = RelayServer::start(0).await.unwrap();
        let r2 = RelayServer::start(0).await.unwrap();
        let base1 = format!("ws://127.0.0.1:{}", r1.port);
        let base2 = format!("ws://127.0.0.1:{}", r2.port);

        let push = push_keyframe(&base1, "short-1").await;
        r2.state().start_proxy(&base1, "short-1", None).unwrap();
        // 确认级联转发已通（观看端经 R2 收到关键帧）
        watch_until_keyframe(&base2, "short-1").await;

        drop(push); // 推流端断开 → R1 删流 → 上游观看会话关闭 → 代理自清理
        // 轮询等待清理（跨进程事件传播需要时间）
        for _ in 0..20 {
            if r2.streams().iter().all(|s| s.stream_id != "short-1") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            r2.streams().iter().all(|s| s.stream_id != "short-1"),
            "上游流结束后代理应自动清理"
        );
        assert!(r2.state().proxies().is_empty(), "代理任务表应清空");

        r1.stop().await;
        r2.stop().await;
    }
}
